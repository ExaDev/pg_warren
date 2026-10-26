//! The edges a centrality's shortest paths follow: the selected relations'
//! forward rows (directed), or the undirected simple graph under them.
//!
//! Directed, a node's row is its row of the selected relations: weighted,
//! an edge in several of them is one entry weighing its lightest
//! relation's weight, not their sum, since two parallel edges are no
//! shorter a path than the lighter (§5.3); unweighted, one entry, and
//! parallel edges change no path's length or count of shortest paths. The
//! same holds of `dijkstra`, `dag_longest_path` (the heaviest) and
//! `minimum_spanning_forest`. Undirected, edge directions and self-loops
//! are ignored: unweighted, through the symmetric adjacency;
//! weighted, through symmetric rows in which a pair keeps the lightest of
//! its relations' weights in either direction, as
//! `minimum_spanning_forest` reads them.

use super::super::product::{FoldedRows, Matrix, RowScratch};
use super::super::symmetric::{distinct, selected_rows, Symmetric};
use super::super::{ComputeError, ComputeMemory};
use super::ShortestPaths;
use crate::graph::DenseId;
use crate::interrupts::{Cancellation, Work};

/// The rows a search reads.
pub(super) enum Adjacency<'g> {
    /// The selected relations' forward rows, a pair's relations folded.
    Directed(Matrix<'g>),
    /// Both directions, unweighted.
    Undirected(Symmetric),
    /// Both directions, with the lightest weight of a pair.
    UndirectedWeighted(FoldedRows),
}

impl<'g> Adjacency<'g> {
    /// The matrix the adjacency reads (whose weightedness it takes): a cost
    /// reading's ([`Matrix::costs`]).
    pub(super) fn matrix(
        paths: &ShortestPaths<'_, 'g>,
        memory: &ComputeMemory,
    ) -> Result<Matrix<'g>, ComputeError> {
        Matrix::costs(paths.graph, paths.relations, paths.weighted, memory)
    }

    /// At most the bytes [`Self::new`] charges, beyond the row scratch a
    /// multi-relation matrix grows on the pool.
    pub(super) fn bytes(paths: &ShortestPaths<'_, 'g>, matrix: &Matrix<'g>) -> u64 {
        let id_space = paths.tombstones.id_space();
        match (paths.directed, matrix.is_weighted()) {
            (true, _) => matrix.folded_bytes(),
            (false, false) => {
                let forward = selected_rows(paths.graph, paths.relations).unwrap_or_default();
                Symmetric::bytes(&forward, id_space)
            }
            // Each stored entry in both directions.
            (false, true) => FoldedRows::bytes(id_space, matrix.stored_entries().saturating_mul(2)),
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
            (true, _) => Self::Directed(matrix.folded(lighter, cancellation)?),
            (false, false) => {
                let forward = selected_rows(paths.graph, paths.relations)?;
                Self::Undirected(Symmetric::new(memory, cancellation, &forward, id_space)?)
            }
            (false, true) => {
                let each_edge = |edge: &mut dyn FnMut(DenseId, DenseId, f64)| {
                    let mut work = Work::new(cancellation);
                    matrix.each_stored_entry(&mut work, &mut |u, v, weight| {
                        if u != v {
                            edge(u, v, weight);
                            edge(v, u, weight);
                        }
                    })
                };
                let rows = FoldedRows::new(memory, cancellation, id_space, each_edge, lighter)?;
                Self::UndirectedWeighted(rows)
            }
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
            Self::UndirectedWeighted(rows) => {
                for &(target, weight) in rows.row(node) {
                    work.spend(1)?;
                    edge(target, weight)?;
                }
                Ok(())
            }
        }
    }
}

/// The lighter of two weights, or NaN if either is, so a NaN weight in any
/// relation or direction is refused.
fn lighter(a: f64, b: f64) -> f64 {
    match a.is_nan() || b.is_nan() {
        true => f64::NAN,
        false => a.min(b),
    }
}
