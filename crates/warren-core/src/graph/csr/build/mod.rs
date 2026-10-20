//! CSR construction without a comparison sort of the edge list (DESIGN.md
//! §5.4 step 4), streamed: relations are built a group at a time by a
//! counting sort of the edge list by source, each row then sorted and
//! deduplicated in place, transposed for the reverse CSR, and handed to a
//! [`CsrSink`] that can write it out and free it before the next relation.
//! The merged CSR comes last, from the edge list itself, which it frees
//! column by column. A [`BuildPlan`] checks the edges and estimates the
//! build's peak memory before anything is built, and every buffer is
//! charged to the build's [`crate::graph::BuildMemory`].

mod columns;
mod deduplication;
mod estimate;
mod groups;
mod sink;

use std::ops::Range;

use columns::{group_rows, merged_rows, Columns};
pub use sink::{BuiltCsr, CsrDirection, CsrScope, CsrSink};

use super::transpose::transpose_within;
use super::{AnyCsr, Csr, CsrLayout, Target, TargetWidth};
use crate::graph::edge_list::{EdgeColumns, Endpoints};
use crate::graph::{BuildBuffer, BuildCharge, BuildMemory, EdgeList, GraphError};

/// Whether a build of `relation_count` relations stores the merged CSR:
/// always without relations (its empty CSR carries the id space and the
/// reverse CSR's presence), never with one, else as `layout` says.
pub(super) fn stores_merged(relation_count: usize, layout: CsrLayout) -> bool {
    match relation_count {
        0 => true,
        1 => false,
        _ => layout.store_merged,
    }
}

/// What a build reports besides its CSRs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BuildSummary {
    /// The edges of every relation's forward CSR, after deduplication.
    pub edge_count: u64,
    /// Parallel edges folded into another edge of the same relation.
    pub deduplicated_edges: u64,
}

/// A build of one edge list, checked and sized before it starts: every
/// edge lies in the id space and names a relation of the dictionary, and
/// [`Self::bytes_needed`] is the most build memory it will hold beyond the
/// edge list's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPlan {
    id_space: u64,
    weighted: bool,
    build_reverse: bool,
    store_merged: bool,
    /// The edge list's length and heap bytes, and the bytes of an endpoint.
    edges: u64,
    edge_bytes: u64,
    endpoint_bytes: u64,
    /// Each relation's edges, before deduplication.
    relation_edges: BuildBuffer<u64>,
    groups: Vec<Range<usize>>,
    /// The most edges from one source, over all relations: a bound on any
    /// row before deduplication.
    longest_row: u64,
}

impl BuildPlan {
    /// The plan of building `edges`, whose relations index a dictionary of
    /// `relation_count` relations and whose dense ids lie in `id_space`, as
    /// `layout` says. Counting edges per source takes `8·id_space` bytes of
    /// `memory` while the plan is made.
    pub fn new(
        edges: &EdgeList,
        relation_count: usize,
        id_space: u64,
        layout: CsrLayout,
        memory: &BuildMemory,
    ) -> Result<Self, GraphError> {
        if id_space == u64::MAX {
            return Err(GraphError::IdSpaceTooLarge);
        }
        let (relations, endpoints) = edges.columns();
        let (relation_edges, longest_row) = match endpoints {
            Endpoints::U32 { sources, targets } => {
                counted(relations, sources, targets, relation_count, id_space, memory)
            }
            Endpoints::U64 { sources, targets } => {
                counted(relations, sources, targets, relation_count, id_space, memory)
            }
        }?;
        let weighted = edges.weights().is_some();
        let target_bytes = target_bytes(id_space);
        let entry_bytes = target_bytes + if weighted { 8 } else { 0 };
        let plan = Self {
            id_space,
            weighted,
            build_reverse: layout.build_reverse,
            store_merged: stores_merged(relation_count, layout),
            edges: edges.len() as u64,
            edge_bytes: edges.bytes(),
            endpoint_bytes: match edges.width() {
                TargetWidth::U32 => 4,
                TargetWidth::U64 => 8,
            },
            groups: groups::groups(&relation_edges, id_space, entry_bytes),
            relation_edges,
            longest_row,
        };
        Ok(plan)
    }

    /// The most build memory the build will hold at once beyond what the
    /// edge list holds now: what to check the budget for before starting.
    /// It counts the CSRs a sink has not yet freed, at most one relation's
    /// forward and reverse CSRs, but not what the sink itself allocates.
    pub fn bytes_needed(&self) -> u64 {
        self.peak_bytes().saturating_sub(self.edge_bytes)
    }

    /// Builds the CSRs of `edges`, which must be the edge list planned,
    /// consuming it, and passes each to `sink` as it is completed. Targets
    /// are `u32` when the id space is below 2³², else `u64`.
    pub fn build<S: CsrSink>(
        &self,
        edges: EdgeList,
        sink: &mut S,
    ) -> Result<BuildSummary, S::Error> {
        match TargetWidth::for_id_space(self.id_space) {
            TargetWidth::U32 => self.build_with::<u32, S>(edges, sink),
            TargetWidth::U64 => self.build_with::<u64, S>(edges, sink),
        }
    }

    /// [`Self::build`] at target width `T`, which must cover the id space.
    /// Tests use it to reach the `u64` path without a 2³² id space.
    pub(crate) fn build_with<T: Target, S: CsrSink>(
        &self,
        edges: EdgeList,
        sink: &mut S,
    ) -> Result<BuildSummary, S::Error>
    where
        AnyCsr: From<Csr<T>>,
    {
        if T::from_dense(self.id_space.saturating_sub(1)).is_none() {
            return Err(GraphError::IdSpaceExceedsTargetWidth.into());
        }
        let EdgeColumns { relations, endpoints, weights } = edges.into_columns();
        let relations = (self.relation_edges.len() > 1).then_some(relations);
        match endpoints {
            Endpoints::U32 { sources, targets } => {
                self.run::<T, u32, S>(Columns { sources, relations, targets, weights }, sink)
            }
            Endpoints::U64 { sources, targets } => {
                self.run::<T, u64, S>(Columns { sources, relations, targets, weights }, sink)
            }
        }
    }

    fn run<T: Target, E: Target, S: CsrSink>(
        &self,
        mut columns: Columns<E>,
        sink: &mut S,
    ) -> Result<BuildSummary, S::Error>
    where
        AnyCsr: From<Csr<T>>,
    {
        let mut summary = BuildSummary::default();
        for (index, group) in self.groups.iter().enumerate() {
            let release = !self.store_merged && index + 1 == self.groups.len();
            let rows = group_rows::<T, E>(
                &mut columns,
                group,
                &self.relation_edges,
                self.id_space,
                release,
            )?;
            // The charge for the rows' headers is held until every
            // relation of the group is built.
            let (rows, _rows_charge) = rows.into_parts();
            for (relation, rows) in group.clone().zip(rows) {
                let (forward, charge, folded) = rows.into_relation_csr(self.id_space)?;
                summary.edge_count += forward.edge_count();
                summary.deduplicated_edges += folded;
                self.emit(CsrScope::Relation(relation), forward, charge, sink)?;
            }
        }
        if self.store_merged {
            let (forward, charge) =
                merged_rows::<T, E>(columns, self.id_space)?.into_merged_csr(self.id_space)?;
            self.emit(CsrScope::Merged, forward, charge, sink)?;
        }
        Ok(summary)
    }

    /// Transposes `forward` if the build has reverse CSRs, then passes both
    /// to `sink`, forward first.
    fn emit<T: Target, S: CsrSink>(
        &self,
        scope: CsrScope,
        forward: Csr<T>,
        charge: BuildCharge,
        sink: &mut S,
    ) -> Result<(), S::Error>
    where
        AnyCsr: From<Csr<T>>,
    {
        let reverse = match self.build_reverse {
            true => Some(transpose_within(&forward, charge.memory())?),
            false => None,
        };
        let direction = CsrDirection::Forward;
        sink.csr(BuiltCsr { scope, direction, csr: forward.into(), charge })?;
        if let Some((reverse, charge)) = reverse {
            let direction = CsrDirection::Reverse;
            sink.csr(BuiltCsr { scope, direction, csr: reverse.into(), charge })?;
        }
        Ok(())
    }
}

/// Each relation's edges and the most edges from one source, after
/// checking that every edge lies in the id space and names a relation of
/// the dictionary (the first edge that does not is the error).
fn counted<S: Target>(
    relations: &[crate::graph::RelationIndex],
    sources: &[S],
    targets: &[S],
    relation_count: usize,
    id_space: u64,
    memory: &BuildMemory,
) -> Result<(BuildBuffer<u64>, u64), GraphError> {
    let rows = usize::try_from(id_space).map_err(|_| GraphError::IdSpaceTooLarge)?;
    let mut degrees = BuildBuffer::filled(memory, rows, 0u64)?;
    let mut relation_edges = BuildBuffer::filled(memory, relation_count, 0u64)?;
    for ((relation, source), target) in relations.iter().zip(sources).zip(targets) {
        let (source, target) = (source.dense(), target.dense());
        if source >= id_space || target >= id_space {
            return Err(GraphError::EdgeOutsideIdSpace { from: source, to: target });
        }
        let Some(count) = relation_edges.get_mut(relation.index()) else {
            return Err(GraphError::RelationOutsideDictionary {
                relation: relation.index() as u64,
            });
        };
        *count += 1;
        degrees[source as usize] += 1;
    }
    Ok((relation_edges, degrees.iter().copied().max().unwrap_or(0)))
}

/// The bytes of one target in the CSRs of `id_space`.
fn target_bytes(id_space: u64) -> u64 {
    match TargetWidth::for_id_space(id_space) {
        TargetWidth::U32 => 4,
        TargetWidth::U64 => 8,
    }
}
