//! The edges a centrality's shortest paths follow: the selected relations'
//! forward rows (directed), or the undirected simple graph under them.
//!
//! Directed, a node's row is row `i` of `A` (§6.3): an edge in several
//! selected relations is one entry whose weight is their sum, as for
//! products and `dag_longest_path`. Undirected, edge directions and
//! self-loops are ignored: unweighted, through `triangles`' symmetric
//! adjacency; weighted, through a symmetric copy of `A`'s entries in which
//! a pair joined both ways keeps the lighter of `A[i][j]` and `A[j][i]`,
//! as `minimum_spanning_forest` reads them.

use rayon::prelude::*;

use super::super::product::{Matrix, RowScratch};
use super::super::symmetric::{distinct, selected_rows, Symmetric};
use super::super::{ComputeError, ComputeMemory, MemoryCharge};
use super::ShortestPaths;
use crate::graph::DenseId;
use crate::interrupts::{Cancellation, Cancelled, Work};

/// The rows a search reads.
pub(super) enum Adjacency<'g> {
    /// The selected relations' forward rows.
    Directed(Matrix<'g>),
    /// Both directions, unweighted.
    Undirected(Symmetric),
    /// Both directions, with the lighter weight of a pair.
    UndirectedWeighted(WeightedSymmetric),
}

impl<'g> Adjacency<'g> {
    /// The matrix the adjacency reads (whose weightedness it takes).
    pub(super) fn matrix(
        paths: &ShortestPaths<'_, 'g>,
        memory: &ComputeMemory,
    ) -> Result<Matrix<'g>, ComputeError> {
        Matrix::new(paths.graph, paths.relations, false, paths.weighted, memory)
    }

    /// At most the bytes [`Self::new`] charges, beyond the row scratch a
    /// multi-relation matrix grows on the pool.
    pub(super) fn bytes(paths: &ShortestPaths<'_, 'g>, matrix: &Matrix<'g>) -> u64 {
        if paths.directed {
            return 0;
        }
        let forward = selected_rows(paths.graph, paths.relations).unwrap_or_default();
        let id_space = paths.tombstones.id_space();
        let symmetric = Symmetric::bytes(&forward, id_space);
        match matrix.is_weighted() {
            // Two 16-byte entries per stored edge, against Symmetric's 8.
            true => symmetric.saturating_mul(2),
            false => symmetric,
        }
    }

    /// The adjacency `paths` asks for, over `matrix`'s entries.
    pub(super) fn new(
        paths: &ShortestPaths<'_, 'g>,
        matrix: Matrix<'g>,
        memory: &ComputeMemory,
        cancellation: &Cancellation,
    ) -> Result<Self, ComputeError> {
        let id_space = paths.tombstones.id_space();
        Ok(match (paths.directed, matrix.is_weighted()) {
            (true, _) => Self::Directed(matrix),
            (false, false) => {
                let forward = selected_rows(paths.graph, paths.relations)?;
                Self::Undirected(Symmetric::new(memory, cancellation, &forward, id_space)?)
            }
            (false, true) => Self::UndirectedWeighted(WeightedSymmetric::new(
                memory,
                cancellation,
                &matrix,
                id_space,
            )?),
        })
    }

    /// Whether the rows carry weights: shortest paths are then the
    /// lightest, else the fewest edges.
    pub(super) fn is_weighted(&self) -> bool {
        match self {
            Self::Directed(matrix) => matrix.is_weighted(),
            Self::Undirected(_) => false,
            Self::UndirectedWeighted(_) => true,
        }
    }

    /// Calls `edge(target, weight)` for each distinct neighbour of `node`
    /// (weight 1 when unweighted), spending one unit of `work` per entry.
    pub(super) fn for_each(
        &self,
        node: DenseId,
        scratch: &mut RowScratch,
        work: &mut Work<'_>,
        mut edge: impl FnMut(DenseId, f64) -> Result<(), ComputeError>,
    ) -> Result<(), ComputeError> {
        match self {
            Self::Directed(matrix) => matrix.for_each_entry(node, scratch, |target, weight| {
                work.spend(1)?;
                edge(target, weight)
            }),
            Self::Undirected(symmetric) => {
                for target in distinct(symmetric.row(node as usize)) {
                    work.spend(1)?;
                    edge(target, 1.0)?;
                }
                Ok(())
            }
            Self::UndirectedWeighted(symmetric) => {
                for &(target, weight) in symmetric.row(node as usize) {
                    work.spend(1)?;
                    edge(target, weight)?;
                }
                Ok(())
            }
        }
    }
}

/// The nodes one sort task handles; a task checks the cancellation flag
/// first. Small under test, so tests cross task boundaries.
const NODES_PER_TASK: usize = if cfg!(test) { 3 } else { 4096 };

/// Each node's neighbours in either direction with the lighter weight of
/// the pair (self-loops dropped), one entry per neighbour, rows sorted.
pub(super) struct WeightedSymmetric {
    /// `id_space + 1` positions into `entries`.
    offsets: Vec<u64>,
    entries: Vec<(DenseId, f64)>,
    _charges: [MemoryCharge; 2],
}

impl WeightedSymmetric {
    fn new(
        memory: &ComputeMemory,
        cancellation: &Cancellation,
        matrix: &Matrix<'_>,
        id_space: u64,
    ) -> Result<Self, ComputeError> {
        let mut scratch = matrix.scratch()?;
        let mut work = Work::new(cancellation);
        let mut each_edge = |f: &mut dyn FnMut(DenseId, DenseId, f64)| {
            for u in 0..id_space {
                work.spend(1)?;
                matrix.for_each_entry(u, &mut scratch, |v, weight| {
                    if v != u {
                        f(u, v, weight);
                    }
                    Ok(work.spend(1)?)
                })?;
            }
            Ok::<_, ComputeError>(())
        };
        let (mut offsets, offsets_charge) = memory.filled(id_space as usize + 1, 0u64)?;
        each_edge(&mut |u, v, _| {
            offsets[u as usize] += 1;
            offsets[v as usize] += 1;
        })?;
        let mut end = 0;
        for offset in &mut offsets[..id_space as usize] {
            end += *offset;
            *offset = end;
        }
        offsets[id_space as usize] = end;
        let (mut entries, entries_charge) = memory.filled(end as usize, (0 as DenseId, 0.0))?;
        each_edge(&mut |u, v, weight| {
            for (row, target) in [(u, v), (v, u)] {
                let offset = &mut offsets[row as usize];
                *offset -= 1;
                entries[*offset as usize] = (target, weight);
            }
        })?;
        let mut symmetric = Self { offsets, entries, _charges: [offsets_charge, entries_charge] };
        symmetric.sort(cancellation)?;
        Ok(symmetric)
    }

    /// The row of `node`.
    fn row(&self, node: usize) -> &[(DenseId, f64)] {
        &self.entries[self.offsets[node] as usize..self.offsets[node + 1] as usize]
    }

    /// Sorts every row by neighbour then weight and keeps the lightest
    /// entry of each neighbour (moving it to the front of a run, so the
    /// row keeps its length: later duplicates are marked by
    /// [`DenseId::MAX`] and compacted out once all rows are sorted).
    fn sort(&mut self, cancellation: &Cancellation) -> Result<(), Cancelled> {
        let offsets = &self.offsets;
        let nodes = offsets.len() - 1;
        let mut tasks = Vec::new();
        let mut rest = self.entries.as_mut_slice();
        for first in (0..nodes).step_by(NODES_PER_TASK) {
            let last = nodes.min(first + NODES_PER_TASK);
            let len = (offsets[last] - offsets[first]) as usize;
            let (chunk, tail) = std::mem::take(&mut rest).split_at_mut(len);
            tasks.push((first..last, chunk));
            rest = tail;
        }
        tasks.into_par_iter().try_for_each(|(nodes, chunk)| {
            if cancellation.is_cancelled() {
                return Err(Cancelled);
            }
            let base = offsets[nodes.start];
            for node in nodes {
                let row = (offsets[node] - base) as usize..(offsets[node + 1] - base) as usize;
                let row = &mut chunk[row];
                row.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
                for i in (1..row.len()).rev() {
                    if row[i].0 == row[i - 1].0 {
                        row[i].0 = DenseId::MAX;
                    }
                }
            }
            Ok(())
        })?;
        self.compact(cancellation)
    }

    /// Drops the duplicates [`Self::sort`] marked, moving rows down.
    fn compact(&mut self, cancellation: &Cancellation) -> Result<(), Cancelled> {
        let mut work = Work::new(cancellation);
        let mut kept = 0;
        let mut start = 0;
        for node in 0..self.offsets.len() - 1 {
            let end = self.offsets[node + 1] as usize;
            self.offsets[node] = kept as u64;
            for i in start..end {
                work.spend(1)?;
                if self.entries[i].0 != DenseId::MAX {
                    self.entries[kept] = self.entries[i];
                    kept += 1;
                }
            }
            start = end;
        }
        let nodes = self.offsets.len() - 1;
        self.offsets[nodes] = kept as u64;
        self.entries.truncate(kept);
        Ok(())
    }
}
