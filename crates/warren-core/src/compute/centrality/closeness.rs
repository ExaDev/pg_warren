//! Closeness and harmonic centrality (DESIGN.md §6.7,
//! `warren.closeness_centrality`, `warren.harmonic_centrality`), over the
//! distances *to* each node, as NetworkX's: a search from each source `s`
//! gives `d(s, u)` for every node `u` it reaches (on a directed graph,
//! NetworkX's incoming distance; on an undirected one either way).
//!
//! From every live node they are exact: closeness
//! `(r − 1) / Σ d(v, u)`, times `(r − 1) / (n − 1)` with `wf_improved`
//! (Wasserman and Faust), where `r − 1` nodes reach `u`; harmonic
//! `Σ 1 / d(v, u)`. From a sample `S`, each is estimated from the
//! `k_u = |S \ {u}|` sources other than `u`, of which `m_u` reach it:
//! closeness `m_u / Σ_{s ∈ S} d(s, u)`, its Wasserman–Faust factor
//! `m_u / k_u`, and harmonic `(n − 1) / k_u · Σ_{s ∈ S} 1 / d(s, u)`. Each is
//! the exact formula with the sample's mean standing for the population's
//! (Eppstein and Wang 2004), so a sample of every live node is exact.

use rayon::prelude::*;

use super::super::{ComputeError, ComputeMemory, MemoryCharge, Vector, Workers};
use super::paths::Paths;
use super::sources::Sources;
use super::{run, Centrality, Measure, Sampling, ShortestPaths, NODES_PER_TASK};
use crate::graph::DenseId;
use crate::interrupts::{Cancellation, Cancelled, Interrupts};

/// Each live node's closeness centrality over the shortest paths of
/// `paths` from the sources `sampling` chooses, scaled by the fraction of
/// nodes that reach it if `wf_improved`.
pub fn closeness_centrality<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    paths: &ShortestPaths<'_, '_>,
    sampling: &Sampling<'_>,
    wf_improved: bool,
) -> Result<Centrality, ComputeError> {
    run(workers, memory, interrupts, paths, sampling, &Closeness { wf_improved })
}

/// Each live node's harmonic centrality over the shortest paths of `paths`
/// from the sources `sampling` chooses.
pub fn harmonic_centrality<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    paths: &ShortestPaths<'_, '_>,
    sampling: &Sampling<'_>,
) -> Result<Centrality, ComputeError> {
    run(workers, memory, interrupts, paths, sampling, &Harmonic)
}

/// Calls `add(u, d(s, u))` for each node `u` of the task starting at
/// `first` and each source `s ≠ u` of `round` that reaches it, sources in
/// order.
fn each_distance(round: &[Paths], first: usize, len: usize, mut add: impl FnMut(usize, f64)) {
    for paths in round {
        let source = paths.source() as usize;
        for (offset, &distance) in paths.distance()[first..first + len].iter().enumerate() {
            if distance.is_finite() && first + offset != source {
                add(offset, distance);
            }
        }
    }
}

/// The sources other than `node`: `|S \ {node}|`.
fn other_sources(sources: &[DenseId], node: DenseId) -> u64 {
    sources.len() as u64 - u64::from(sources.binary_search(&node).is_ok())
}

struct Closeness {
    wf_improved: bool,
}

/// Each node's summed distance from the sources that reach it, and their
/// number.
struct Reach {
    distances: Vector<f64>,
    reached: Vec<u64>,
    _reached: MemoryCharge,
}

impl Measure for Closeness {
    const COUNTED: bool = false;
    type Totals = Reach;

    fn totals(&self, memory: &ComputeMemory, id_space: u64) -> Result<Reach, ComputeError> {
        let (reached, charge) = memory.filled(id_space as usize, 0u64)?;
        let distances = Vector::filled(memory, id_space, 1, 0.0)?;
        Ok(Reach { distances, reached, _reached: charge })
    }

    fn fold(
        &self,
        totals: &mut Reach,
        round: &[Paths],
        cancellation: &Cancellation,
    ) -> Result<(), Cancelled> {
        let distances = totals.distances.values_mut().par_chunks_mut(NODES_PER_TASK);
        distances.zip(totals.reached.par_chunks_mut(NODES_PER_TASK)).enumerate().try_for_each(
            |(task, (distances, reached))| {
                if cancellation.is_cancelled() {
                    return Err(Cancelled);
                }
                each_distance(round, task * NODES_PER_TASK, distances.len(), |offset, d| {
                    distances[offset] += d;
                    reached[offset] += 1;
                });
                Ok(())
            },
        )
    }

    fn finish(
        &self,
        totals: Reach,
        sources: &Sources,
        live: u64,
    ) -> Result<Vector<f64>, ComputeError> {
        let Reach { mut distances, reached, .. } = totals;
        for ((node, total), &reached) in (0..).zip(distances.values_mut()).zip(&reached) {
            let r = reached as f64;
            *total = match *total > 0.0 && live > 1 {
                true if self.wf_improved => {
                    r / *total * (r / other_sources(sources.nodes(), node) as f64)
                }
                true => r / *total,
                false => 0.0,
            };
        }
        Ok(distances)
    }
}

struct Harmonic;

impl Measure for Harmonic {
    const COUNTED: bool = false;
    type Totals = Vector<f64>;

    fn totals(&self, memory: &ComputeMemory, id_space: u64) -> Result<Vector<f64>, ComputeError> {
        Vector::filled(memory, id_space, 1, 0.0)
    }

    fn fold(
        &self,
        totals: &mut Vector<f64>,
        round: &[Paths],
        cancellation: &Cancellation,
    ) -> Result<(), Cancelled> {
        totals.values_mut().par_chunks_mut(NODES_PER_TASK).enumerate().try_for_each(
            |(task, totals)| {
                if cancellation.is_cancelled() {
                    return Err(Cancelled);
                }
                each_distance(round, task * NODES_PER_TASK, totals.len(), |offset, d| {
                    totals[offset] += 1.0 / d;
                });
                Ok(())
            },
        )
    }

    fn finish(
        &self,
        mut totals: Vector<f64>,
        sources: &Sources,
        live: u64,
    ) -> Result<Vector<f64>, ComputeError> {
        if !sources.every_live_node() {
            for (node, total) in (0..).zip(totals.values_mut()) {
                let others = other_sources(sources.nodes(), node);
                *total = match others {
                    0 => 0.0,
                    _ => *total * ((live - 1) as f64 / others as f64),
                };
            }
        }
        Ok(totals)
    }
}
