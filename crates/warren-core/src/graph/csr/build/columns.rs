//! The edge list's columns as a build consumes them, and their counting
//! sort into rows, with no comparison sort of the edges. A relation's rows
//! are placed through the build's [`RowCursors`], one relation at a time
//! (a group's edges first ordered by relation), so its offsets can be
//! sparse. The dense rows of the merged CSR and of a graph's only relation
//! use their offsets as the cursors:
//! counts go to `offsets[source + 2]`, so after the prefix sum
//! `offsets[source + 1]` is where `source`'s row starts; scattering
//! advances it to where the row ends, which leaves `offsets` exactly the
//! CSR's offsets. A second column is scattered after shifting them back.

use std::ops::Range;

use super::super::cursors::{RowCursors, RowIndex};
use super::super::Target;
use crate::graph::{BuildBuffer, BuildMemory, GraphError, RelationIndex};

/// The edge list's columns, with endpoints of width `S`.
pub(super) struct Columns<S> {
    pub(super) sources: BuildBuffer<S>,
    /// `None` when the dictionary has at most one relation, so every edge
    /// is relation 0.
    pub(super) relations: Option<BuildBuffer<RelationIndex>>,
    pub(super) targets: BuildBuffer<S>,
    pub(super) weights: Option<BuildBuffer<f64>>,
}

impl<S: Target> Columns<S> {
    /// The relation of `edge`.
    fn relation(&self, edge: usize) -> usize {
        self.relations.as_ref().map_or(0, |relations| relations[edge].index())
    }

    /// Frees every column.
    fn release(&mut self) {
        self.sources.release();
        self.targets.release();
        if let Some(relations) = &mut self.relations {
            relations.release();
        }
        if let Some(weights) = &mut self.weights {
            weights.release();
        }
    }
}

/// Edges grouped into rows by source: entries `offsets[i]..offsets[i + 1]`
/// of `targets` (and `weights`, `relations`) belong to source `i`, or with
/// sparse offsets to the `i`th of `rows`, in input order.
pub(super) struct Rows<T> {
    /// The stored rows' ids, with sparse offsets.
    pub(super) rows: Option<BuildBuffer<T>>,
    pub(super) offsets: BuildBuffer<u64>,
    pub(super) targets: BuildBuffer<T>,
    pub(super) weights: Option<BuildBuffer<f64>>,
    /// Each entry's relation, kept only for the merged CSR's weights.
    pub(super) relations: Option<BuildBuffer<RelationIndex>>,
    /// Whether a column has been scattered, so `offsets` are the CSR's
    /// rather than the scatter cursors.
    scattered: bool,
}

impl<T: Target> Rows<T> {
    /// Zeroed offsets for `id_space` rows and room for `edges` targets.
    fn new(memory: &BuildMemory, id_space: u64, edges: u64) -> Result<Self, GraphError> {
        Ok(Self {
            rows: None,
            offsets: BuildBuffer::filled(memory, row_count(id_space)? + 1, 0)?,
            targets: BuildBuffer::filled(memory, length(edges)?, T::default())?,
            weights: None,
            relations: None,
            scattered: false,
        })
    }
}

/// The rows of the relations in `group`, one [`Rows`] each, placed from
/// `columns` through `cursors`, which are left zeroed. With `release`, the
/// targets column is freed once scattered, and the rest once all are: the
/// edge list is then no longer needed.
pub(super) fn group_rows<T: Target, S: Target>(
    columns: &mut Columns<S>,
    group: &Range<usize>,
    relation_edges: &[u64],
    cursors: &mut RowCursors,
    release: bool,
) -> Result<BuildBuffer<Rows<T>>, GraphError> {
    let memory = columns.sources.memory().clone();
    let order = RelationOrder::new(columns, group, relation_edges)?;
    let mut rows = BuildBuffer::with_capacity(&memory, group.len())?;
    // Weights go with the targets unless the targets column is freed first.
    let together = columns.weights.is_some() && !release;
    for relation in group.clone() {
        let edges = || order.edges(columns, group, relation);
        let index = cursors.index::<T, _>(true, || edges().map(|e| columns.sources[e].dense()))?;
        let length = length(relation_edges[relation])?;
        let mut targets = BuildBuffer::filled(&memory, length, T::default())?;
        let mut weights = match together {
            true => Some(BuildBuffer::filled(&memory, length, 0.0)?),
            false => None,
        };
        for edge in edges() {
            let at = cursors.place(columns.sources[edge].dense());
            targets[at] = target::<T, S>(columns.targets[edge]);
            if let (Some(out), Some(input)) = (&mut weights, &columns.weights) {
                out[at] = input[edge];
            }
        }
        cursors.clear(index.rows.as_deref(), &index.offsets);
        let RowIndex { rows: stored, offsets } = index;
        let (relations, scattered) = (None, true);
        rows.push(Rows { rows: stored, offsets, targets, weights, relations, scattered })?;
    }
    if release {
        columns.targets.release();
    }
    if let (Some(input), false) = (&columns.weights, together) {
        for (relation, rows) in group.clone().zip(rows.iter_mut()) {
            let mut weights = BuildBuffer::filled(&memory, length(relation_edges[relation])?, 0.0)?;
            cursors.rewind(rows.rows.as_deref(), &rows.offsets);
            for edge in order.edges(columns, group, relation) {
                weights[cursors.place(columns.sources[edge].dense())] = input[edge];
            }
            cursors.clear(rows.rows.as_deref(), &rows.offsets);
            rows.weights = Some(weights);
        }
    }
    drop(order);
    if release {
        columns.release();
    }
    Ok(rows)
}

/// The edges of a group of relations ordered by relation, stably, so that
/// each relation's edges are read in input order without a pass over the
/// whole edge list; a group of one relation reads the edge list itself.
enum RelationOrder {
    /// One relation: every edge of the list, filtered by relation.
    One,
    /// Each relation's edges, the group's relations one after another.
    Narrow { order: BuildBuffer<u32>, starts: BuildBuffer<usize> },
    /// As `Narrow`, for an edge list of 2³² edges or more.
    Wide { order: BuildBuffer<u64>, starts: BuildBuffer<usize> },
}

impl RelationOrder {
    /// The order of `group`'s edges in `columns`, by a counting sort.
    fn new<S: Target>(
        columns: &Columns<S>,
        group: &Range<usize>,
        relation_edges: &[u64],
    ) -> Result<Self, GraphError> {
        if group.len() <= 1 {
            return Ok(Self::One);
        }
        let memory = columns.sources.memory();
        let mut starts = BuildBuffer::with_capacity(memory, group.len() + 1)?;
        let mut end = 0usize;
        starts.push(end)?;
        for &edges in &relation_edges[group.clone()] {
            end += length(edges)?;
            starts.push(end)?;
        }
        let mut cursors = BuildBuffer::with_capacity(memory, group.len())?;
        starts[..group.len()].iter().try_for_each(|&start| cursors.push(start))?;
        let edges = 0..columns.sources.len();
        let of_group = edges.filter_map(|edge| {
            let relation = columns.relation(edge);
            group.contains(&relation).then(|| (relation - group.start, edge))
        });
        match u32::try_from(columns.sources.len()) {
            Ok(_) => {
                let mut order = BuildBuffer::filled(memory, end, 0u32)?;
                for (slot, edge) in of_group {
                    order[cursors[slot]] = edge as u32;
                    cursors[slot] += 1;
                }
                Ok(Self::Narrow { order, starts })
            }
            Err(_) => {
                let mut order = BuildBuffer::filled(memory, end, 0u64)?;
                for (slot, edge) in of_group {
                    order[cursors[slot]] = edge as u64;
                    cursors[slot] += 1;
                }
                Ok(Self::Wide { order, starts })
            }
        }
    }

    /// The edges of `relation`, of `group`, in input order.
    fn edges<'a, S: Target>(
        &'a self,
        columns: &'a Columns<S>,
        group: &Range<usize>,
        relation: usize,
    ) -> Edges<'a, S> {
        let slot = relation - group.start;
        match self {
            Self::One => Edges::Filtered { columns, relation, next: 0 },
            Self::Narrow { order, starts } => {
                Edges::Narrow(order[starts[slot]..starts[slot + 1]].iter())
            }
            Self::Wide { order, starts } => {
                Edges::Wide(order[starts[slot]..starts[slot + 1]].iter())
            }
        }
    }
}

/// The edges of one relation, as positions in the edge list.
enum Edges<'a, S> {
    Filtered { columns: &'a Columns<S>, relation: usize, next: usize },
    Narrow(std::slice::Iter<'a, u32>),
    Wide(std::slice::Iter<'a, u64>),
}

impl<S: Target> Iterator for Edges<'_, S> {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<usize> {
        match self {
            Self::Filtered { columns, relation, next } => {
                while *next < columns.sources.len() {
                    let edge = *next;
                    *next += 1;
                    if columns.relation(edge) == *relation {
                        return Some(edge);
                    }
                }
                None
            }
            Self::Narrow(order) => order.next().map(|&edge| edge as usize),
            Self::Wide(order) => order.next().map(|&edge| edge as usize),
        }
    }
}

/// The rows of every edge with dense offsets, for the merged CSR or a
/// graph's only relation, scattered from `columns`, which are freed one by
/// one as they are scattered. With several relations, each entry keeps its
/// relation when weighted, to sum weights per relation first.
pub(super) fn dense_rows<T: Target, S: Target>(
    mut columns: Columns<S>,
    id_space: u64,
) -> Result<Rows<T>, GraphError> {
    let memory = columns.sources.memory().clone();
    let edges = columns.sources.len();
    let mut rows = [Rows::new(&memory, id_space, edges as u64)?];
    let all = |_| Some(0);
    count(&columns.sources, all, &mut rows);
    scatter(&columns.sources, all, &mut rows, |rows, at, edge| {
        rows.targets[at] = target::<T, S>(columns.targets[edge]);
    });
    columns.targets.release();
    if let Some(input) = &columns.weights {
        if let Some(relations) = &columns.relations {
            rows[0].relations =
                Some(BuildBuffer::filled(&memory, edges, RelationIndex::default())?);
            scatter(&columns.sources, all, &mut rows, |rows, at, edge| {
                if let Some(out) = &mut rows.relations {
                    out[at] = relations[edge];
                }
            });
        }
        if let Some(relations) = &mut columns.relations {
            relations.release();
        }
        rows[0].weights = Some(BuildBuffer::filled(&memory, edges, 0.0)?);
        scatter(&columns.sources, all, &mut rows, |rows, at, edge| {
            if let Some(out) = &mut rows.weights {
                out[at] = input[edge];
            }
        });
    }
    columns.release();
    let [rows] = rows;
    Ok(rows)
}

/// Counts each selected edge into `offsets[source + 2]` of its slot's
/// rows, then prefix-sums, so `offsets[source + 1]` is the row's start.
fn count<S: Target, T>(sources: &[S], slot: impl Fn(usize) -> Option<usize>, rows: &mut [Rows<T>]) {
    for (edge, source) in sources.iter().enumerate() {
        if let Some(slot) = slot(edge) {
            if let Some(count) = rows[slot].offsets.get_mut(source.dense() as usize + 2) {
                *count += 1;
            }
        }
    }
    for rows in rows {
        for row in 1..rows.offsets.len() {
            rows.offsets[row] += rows.offsets[row - 1];
        }
    }
}

/// Scatters selected edges into row order, stable in input order: `place`
/// fills the row entry at the given position from the given edge. Leaves
/// each slot's offsets the CSR's; offsets left by an earlier scatter are
/// first shifted back to the row starts.
fn scatter<S: Target, T>(
    sources: &[S],
    slot: impl Fn(usize) -> Option<usize>,
    rows: &mut [Rows<T>],
    mut place: impl FnMut(&mut Rows<T>, usize, usize),
) {
    for rows in rows.iter_mut() {
        if rows.scattered {
            shift(&mut rows.offsets);
        }
        rows.scattered = true;
    }
    for (edge, source) in sources.iter().enumerate() {
        let Some(slot) = slot(edge) else { continue };
        let rows = &mut rows[slot];
        let cursor = &mut rows.offsets[source.dense() as usize + 1];
        let position = *cursor as usize;
        *cursor += 1;
        place(rows, position, edge);
    }
}

/// Turns CSR offsets (`offsets[v]`, the start of row `v`) back into scatter
/// cursors (`offsets[v + 1]`, the start of row `v`).
fn shift(offsets: &mut [u64]) {
    let rows = offsets.len() - 1;
    offsets.copy_within(0..rows, 1);
    offsets[0] = 0;
}

/// A source column's target at the CSR's target width; the plan checked
/// that every endpoint lies in the id space, which the width covers.
fn target<T: Target, S: Target>(target: S) -> T {
    T::from_dense(target.dense()).unwrap_or_default()
}

/// The number of rows of an id space, as an index.
fn row_count(id_space: u64) -> Result<usize, GraphError> {
    usize::try_from(id_space).map_err(|_| GraphError::IdSpaceTooLarge)
}

/// An edge count as a length.
fn length(edges: u64) -> Result<usize, GraphError> {
    usize::try_from(edges).map_err(|_| GraphError::OutOfMemory { elements: usize::MAX })
}
