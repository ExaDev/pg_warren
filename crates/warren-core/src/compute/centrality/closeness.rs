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
//!
//! On unweighted rows the sources are searched in batches, which count
//! for each node how many sources first reach it at each distance `d`:
//! closeness then sums distances as integers, and harmonic adds
//! `count / d` in order of batch, then distance.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use rayon::prelude::*;

use super::super::{ComputeError, ComputeMemory, MemoryCharge, Vector, Workers};
use super::paths::Paths;
use super::sources::Sources;
use super::{run_batched, Batched, Centrality, Measure, Sampling, ShortestPaths, NODES_PER_TASK};
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
    run_batched(workers, memory, interrupts, paths, sampling, &Closeness { wf_improved })
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
    run_batched(workers, memory, interrupts, paths, sampling, &Harmonic)
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

/// `len` zeroed counters, charged to `memory` before they are reserved.
fn counters<T: Default>(
    memory: &ComputeMemory,
    len: u64,
) -> Result<(Vec<T>, MemoryCharge), ComputeError> {
    let charge = memory.charge_elements::<T>(len as usize)?;
    let mut counters = Vec::new();
    counters
        .try_reserve_exact(len as usize)
        .map_err(|_| ComputeError::OutOfMemory { bytes: charge.bytes() })?;
    counters.resize_with(len as usize, T::default);
    Ok((counters, charge))
}

/// Adds `value` to `counter`. Only one thread adds to a node at a time,
/// so a load and a store suffice.
fn add_to(counter: &AtomicU64, value: impl FnOnce(u64) -> u64) {
    counter.store(value(counter.load(Relaxed)), Relaxed);
}

/// The vector of `value(node)` over `id_space` nodes.
fn vector_of(
    memory: &ComputeMemory,
    id_space: u64,
    value: impl Fn(DenseId) -> f64,
) -> Result<Vector<f64>, ComputeError> {
    let mut vector = Vector::filled(memory, id_space, 1, 0.0)?;
    for (node, slot) in (0..).zip(vector.values_mut()?) {
        *slot = value(node);
    }
    Ok(vector)
}

struct Closeness {
    wf_improved: bool,
}

impl Closeness {
    /// The closeness of `node`, which `reached` of `sources` reach at a
    /// summed distance of `total`.
    fn value(&self, node: DenseId, reached: u64, total: f64, sources: &Sources, live: u64) -> f64 {
        let r = reached as f64;
        match total > 0.0 && live > 1 {
            true if self.wf_improved => {
                r / total * (r / other_sources(sources.nodes(), node) as f64)
            }
            true => r / total,
            false => 0.0,
        }
    }
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
    ) -> Result<(), ComputeError> {
        let distances = totals.distances.values_mut()?.par_chunks_mut(NODES_PER_TASK);
        Ok(distances.zip(totals.reached.par_chunks_mut(NODES_PER_TASK)).enumerate().try_for_each(
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
        )?)
    }

    fn finish(
        &self,
        totals: Reach,
        sources: &Sources,
        live: u64,
    ) -> Result<Vector<f64>, ComputeError> {
        let Reach { mut distances, reached, .. } = totals;
        for ((node, total), &reached) in (0..).zip(distances.values_mut()?).zip(&reached) {
            *total = self.value(node, reached, *total, sources, live);
        }
        Ok(distances)
    }
}

/// Each node's number of sources that reach it and their summed distance,
/// as integers, side by side.
struct ReachCounts {
    counts: Vec<[AtomicU64; 2]>,
    _charge: MemoryCharge,
}

impl Batched for Closeness {
    type Counts = ReachCounts;

    fn counts(&self, memory: &ComputeMemory, id_space: u64) -> Result<ReachCounts, ComputeError> {
        let (counts, charge) = counters(memory, id_space)?;
        Ok(ReachCounts { counts, _charge: charge })
    }

    fn add(&self, counts: &ReachCounts, node: DenseId, sources: u32, distance: u32) {
        let [reached, distances] = &counts.counts[node as usize];
        let sources = u64::from(sources);
        add_to(reached, |reached| reached + sources);
        add_to(distances, |total| total + sources * u64::from(distance));
    }

    fn finish_counts(
        &self,
        counts: ReachCounts,
        memory: &ComputeMemory,
        sources: &Sources,
        live: u64,
    ) -> Result<Vector<f64>, ComputeError> {
        let counts = counts.counts;
        vector_of(memory, counts.len() as u64, |node| {
            let [reached, distances] = &counts[node as usize];
            let total = distances.load(Relaxed) as f64;
            self.value(node, reached.load(Relaxed), total, sources, live)
        })
    }
}

struct Harmonic;

impl Harmonic {
    /// The harmonic centrality of `node` from its sum `total` over
    /// `sources`: scaled to every live node if they are a sample.
    fn value(node: DenseId, total: f64, sources: &Sources, live: u64) -> f64 {
        if sources.every_live_node() {
            return total;
        }
        match other_sources(sources.nodes(), node) {
            0 => 0.0,
            others => total * ((live - 1) as f64 / others as f64),
        }
    }
}

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
    ) -> Result<(), ComputeError> {
        Ok(totals.values_mut()?.par_chunks_mut(NODES_PER_TASK).enumerate().try_for_each(
            |(task, totals)| {
                if cancellation.is_cancelled() {
                    return Err(Cancelled);
                }
                each_distance(round, task * NODES_PER_TASK, totals.len(), |offset, d| {
                    totals[offset] += 1.0 / d;
                });
                Ok(())
            },
        )?)
    }

    fn finish(
        &self,
        mut totals: Vector<f64>,
        sources: &Sources,
        live: u64,
    ) -> Result<Vector<f64>, ComputeError> {
        for (node, total) in (0..).zip(totals.values_mut()?) {
            *total = Self::value(node, *total, sources, live);
        }
        Ok(totals)
    }
}

/// Each node's sum, as the bits of an `f64`.
struct Sums {
    sums: Vec<AtomicU64>,
    _charge: MemoryCharge,
}

impl Batched for Harmonic {
    type Counts = Sums;

    fn counts(&self, memory: &ComputeMemory, id_space: u64) -> Result<Sums, ComputeError> {
        let (sums, charge) = counters(memory, id_space)?;
        Ok(Sums { sums, _charge: charge })
    }

    /// Adds `sources / distance`.
    fn add(&self, counts: &Sums, node: DenseId, sources: u32, distance: u32) {
        add_to(&counts.sums[node as usize], |sum| {
            (f64::from_bits(sum) + f64::from(sources) / f64::from(distance)).to_bits()
        });
    }

    fn finish_counts(
        &self,
        counts: Sums,
        memory: &ComputeMemory,
        sources: &Sources,
        live: u64,
    ) -> Result<Vector<f64>, ComputeError> {
        let sums = counts.sums;
        vector_of(memory, sums.len() as u64, |node| {
            Self::value(node, f64::from_bits(sums[node as usize].load(Relaxed)), sources, live)
        })
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use proptest::prelude::*;

    use super::super::run;
    use super::*;
    use crate::compute::reference::{self, Edges};
    use crate::graph::{RelationSelection, Tombstones};
    use crate::interrupts::Uninterrupted;

    /// What `measure` gives one source at a time, and in batches on one
    /// thread and on three, as bits.
    fn both_ways<M: Batched>(
        paths: &ShortestPaths<'_, '_>,
        sampling: &Sampling<'_>,
        measure: &M,
    ) -> (Vec<f64>, [Vec<u64>; 2]) {
        let memory = ComputeMemory::new(1 << 26);
        let on = |threads| Workers::new(threads).unwrap();
        let one_by_one =
            run(&on(3), &memory, &mut Uninterrupted, paths, sampling, measure).unwrap();
        let batched = |threads| {
            let found =
                run_batched(&on(threads), &memory, &mut Uninterrupted, paths, sampling, measure)
                    .unwrap();
            assert_eq!(found.sources, one_by_one.sources);
            found.values.values().iter().map(|v| v.to_bits()).collect()
        };
        (one_by_one.values.values().to_vec(), [batched(1), batched(3)])
    }

    proptest! {
        #[test]
        fn batches_give_what_one_source_at_a_time_gives(
            id_space in 1u64..150,
            edges in prop::collection::vec((0usize..2, 0u64..150, 0u64..150), 0..400),
            only in prop::option::of(0usize..2),
            tombstoned in prop::collection::vec(0u64..150, 0..8),
            k in prop::option::of(1u64..150),
            seed in any::<u64>(),
            directed in any::<bool>(),
        ) {
            let removed = |n: &u64| tombstoned.contains(n);
            let edges: Edges = edges
                .into_iter()
                .filter(|&(_, s, t)| s < id_space && t < id_space && !removed(&s) && !removed(&t))
                .map(|(r, s, t)| (r, s, t, 1.0))
                .collect();
            let graph = reference::graph(id_space, 2, &edges, false, false);
            let mut tombstones = Tombstones::new(id_space).unwrap();
            tombstoned.iter().filter(|&&t| t < id_space).for_each(|&t| tombstones.insert(t).unwrap());
            let indexes = reference::relations(2);
            let relations = match only {
                Some(only) => RelationSelection::Only(&indexes[only..=only]),
                None => RelationSelection::All,
            };
            let paths = ShortestPaths {
                graph: &graph,
                relations,
                tombstones: &tombstones,
                directed,
                weighted: None,
            };
            let sampling = Sampling { k, seed, population: None };
            for wf_improved in [true, false] {
                let (expected, [one, three]) = both_ways(&paths, &sampling, &Closeness { wf_improved });
                let expected: Vec<u64> = expected.iter().map(|v| v.to_bits()).collect();
                prop_assert_eq!(&one, &expected);
                prop_assert_eq!(&three, &expected);
            }
            let (expected, [one, three]) = both_ways(&paths, &sampling, &Harmonic);
            prop_assert_eq!(&one, &three);
            for (found, expected) in one.iter().map(|&v| f64::from_bits(v)).zip(expected) {
                prop_assert!((found - expected).abs() <= 1e-12 * expected.abs().max(1.0));
            }
        }
    }
}
