//! Each node's neighbours in the undirected simple graph under the selected
//! relations, sorted: built once for the whole graph (`triangles`'
//! symmetric adjacency, with every node's degree), or read node by node
//! from the forward and reverse CSRs when every selected relation has a
//! reverse CSR, so a query about a few nodes costs their neighbourhoods,
//! not the graph.

use std::mem::size_of;

use rayon::prelude::*;

use super::super::symmetric::{distinct, selected_rows, Symmetric};
use super::super::{ComputeError, ComputeMemory, MemoryCharge};
use crate::graph::{CsrRows, DenseId, GraphCsr, RelationSelection};
use crate::interrupts::{Cancellation, Cancelled, Work};

/// The nodes one degree task handles; a task checks the cancellation flag
/// first.
const NODES_PER_TASK: usize = if cfg!(test) { 3 } else { 4096 };

/// How a kernel reads neighbourhoods.
pub(super) enum Neighbourhoods<'g> {
    /// The whole symmetric adjacency, and each node's degree.
    Built { symmetric: Symmetric, degrees: Vec<u64>, _charge: MemoryCharge },
    /// Each selected relation's (or the all-relations) rows, both ways.
    Local(Vec<CsrRows<'g>>),
}

impl<'g> Neighbourhoods<'g> {
    /// The rows to read node by node, if every selected relation has a
    /// reverse CSR.
    pub(super) fn local(
        graph: &'g GraphCsr,
        relations: RelationSelection<'_>,
    ) -> Result<Option<Self>, ComputeError> {
        let rows = match relations {
            RelationSelection::All => {
                let all = graph.all_relations();
                all.reverse().map(|reverse| vec![all.forward(), reverse])
            }
            RelationSelection::Only(_) => {
                let selected = graph.selected(relations).map_err(|relation| {
                    ComputeError::UnknownRelation { relation: relation.index() }
                })?;
                selected
                    .iter()
                    .flat_map(|relation| [Some(relation.forward()), relation.reverse()])
                    .map(|csr| csr.map(CsrRows::from))
                    .collect()
            }
        };
        Ok(rows.map(Self::Local))
    }

    /// At most the bytes [`Self::built`] charges.
    pub(super) fn built_bytes(
        graph: &GraphCsr,
        relations: RelationSelection<'_>,
    ) -> Result<u64, ComputeError> {
        let forward = selected_rows(graph, relations)?;
        let id_space = graph.all_relations().forward().id_space();
        let degrees = id_space.saturating_mul(size_of::<u64>() as u64);
        Ok(Symmetric::bytes(&forward, id_space).saturating_add(degrees))
    }

    /// The symmetric adjacency of the whole graph, and every degree.
    pub(super) fn built(
        memory: &ComputeMemory,
        cancellation: &Cancellation,
        graph: &'g GraphCsr,
        relations: RelationSelection<'_>,
    ) -> Result<Self, ComputeError> {
        let forward = selected_rows(graph, relations)?;
        let id_space = graph.all_relations().forward().id_space();
        let symmetric = Symmetric::new(memory, cancellation, &forward, id_space)?;
        let (mut degrees, charge) = memory.filled(id_space as usize, 0u64)?;
        degrees.par_chunks_mut(NODES_PER_TASK).enumerate().try_for_each(|(task, degrees)| {
            if cancellation.is_cancelled() {
                return Err(Cancelled);
            }
            for (offset, degree) in degrees.iter_mut().enumerate() {
                *degree = distinct(symmetric.row(task * NODES_PER_TASK + offset)).count() as u64;
            }
            Ok(())
        })?;
        Ok(Self::Built { symmetric, degrees, _charge: charge })
    }

    /// `node`'s neighbours, ascending (repeating, when built, a neighbour
    /// joined more than once: read them with `distinct` or `shared`).
    pub(super) fn row<'s>(
        &'s self,
        node: DenseId,
        buffer: &'s mut RowBuffer,
        work: &mut Work<'_>,
    ) -> Result<&'s [DenseId], ComputeError> {
        match self {
            Self::Built { symmetric, .. } => {
                let row = symmetric.row(node as usize);
                work.spend(row.len() as u64)?;
                Ok(row)
            }
            Self::Local(rows) => buffer.fill(rows, node, work),
        }
    }

    /// Every node's degree, when built.
    pub(super) fn degrees(&self) -> Option<&[u64]> {
        match self {
            Self::Built { degrees, .. } => Some(degrees),
            Self::Local(_) => None,
        }
    }

    /// The number of `node`'s distinct neighbours, given its row.
    pub(super) fn degree_of_row(&self, node: DenseId, row: &[DenseId]) -> u64 {
        match self {
            Self::Built { degrees, .. } => degrees[node as usize],
            Self::Local(_) => row.len() as u64,
        }
    }

    /// The number of `node`'s distinct neighbours.
    pub(super) fn degree(
        &self,
        node: DenseId,
        buffer: &mut RowBuffer,
        work: &mut Work<'_>,
    ) -> Result<u64, ComputeError> {
        match self {
            Self::Built { degrees, .. } => Ok(degrees[node as usize]),
            Self::Local(rows) => Ok(buffer.fill(rows, node, work)?.len() as u64),
        }
    }
}

/// A buffer one node's neighbours are read into, its capacity charged to
/// the compute budget as it grows.
pub(super) struct RowBuffer {
    neighbours: Vec<DenseId>,
    charge: MemoryCharge,
}

impl RowBuffer {
    /// An empty buffer charged to `memory`.
    pub(super) fn new(memory: &ComputeMemory) -> Result<Self, ComputeError> {
        Ok(Self { neighbours: Vec::new(), charge: memory.charge(0)? })
    }

    /// The values `fill` gives `each` with, at most `at_most` of them.
    pub(super) fn collect(
        &mut self,
        at_most: usize,
        fill: impl FnOnce(&mut dyn FnMut(DenseId)) -> Result<(), Cancelled>,
    ) -> Result<&[DenseId], ComputeError> {
        self.clear_for(at_most as u64)?;
        let neighbours = &mut self.neighbours;
        fill(&mut |value| neighbours.push(value))?;
        Ok(&self.neighbours)
    }

    /// Empties the buffer and makes room for `len` values.
    fn clear_for(&mut self, len: u64) -> Result<(), ComputeError> {
        self.neighbours.clear();
        let capacity = self.neighbours.capacity() as u64;
        if len > capacity {
            let bytes = (len - capacity).saturating_mul(size_of::<DenseId>() as u64);
            self.charge.grow(bytes)?;
            self.neighbours.try_reserve_exact(len as usize).map_err(|_| {
                self.charge.shrink(bytes);
                ComputeError::OutOfMemory { bytes }
            })?;
        }
        Ok(())
    }

    /// `node`'s distinct neighbours other than itself, ascending.
    fn fill(
        &mut self,
        rows: &[CsrRows<'_>],
        node: DenseId,
        work: &mut Work<'_>,
    ) -> Result<&[DenseId], ComputeError> {
        let len: u64 = rows.iter().map(|rows| rows.degree_bound(node)).sum();
        work.spend(len)?;
        self.clear_for(len)?;
        for rows in rows {
            self.neighbours.extend(rows.neighbours(node).filter(|&target| target != node));
        }
        self.neighbours.sort_unstable();
        self.neighbours.dedup();
        Ok(&self.neighbours)
    }
}
