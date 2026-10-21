//! Betweenness centrality (DESIGN.md §6.7, `warren.betweenness_centrality`):
//! for each node, the fraction of shortest paths between other nodes that
//! pass through it, summed over pairs, by Brandes' algorithm from every
//! live node or from `k` sampled sources, rescaled exactly as NetworkX 3.7's
//! `betweenness_centrality(G, k, normalized, weight, endpoints)` rescales.

use rayon::prelude::*;

use super::super::{ComputeError, ComputeMemory, Vector, Workers};
use super::adjacency::Adjacency;
use super::paths::Paths;
use super::sources::Sources;
use super::{run, Centrality, Measure, Sampling, ShortestPaths, NODES_PER_TASK};
use crate::interrupts::{Cancellation, Cancelled, Interrupts, Work};

/// How betweenness is counted and scaled, as NetworkX's arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BetweennessScale {
    /// Divide by the number of pairs a path could join.
    pub normalized: bool,
    /// Count a path's endpoints as on it.
    pub endpoints: bool,
}

/// Each live node's betweenness centrality over the shortest paths of
/// `paths` from the sources `sampling` chooses.
pub fn betweenness_centrality<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    paths: &ShortestPaths<'_, '_>,
    sampling: &Sampling<'_>,
    scale: BetweennessScale,
) -> Result<Centrality, ComputeError> {
    let measure = Betweenness { scale, directed: paths.directed };
    run(workers, memory, interrupts, paths, sampling, &measure)
}

struct Betweenness {
    scale: BetweennessScale,
    directed: bool,
}

impl Measure for Betweenness {
    const COUNTED: bool = true;
    type Totals = Vector<f64>;

    fn totals(&self, memory: &ComputeMemory, id_space: u64) -> Result<Vector<f64>, ComputeError> {
        Vector::filled(memory, id_space, 1, 0.0)
    }

    /// Each node's dependency on the source, plus one for a path's end if
    /// `endpoints`; the source gains nothing, or with `endpoints` one per
    /// node it reaches.
    fn after_search(
        &self,
        paths: &mut Paths,
        adjacency: &Adjacency<'_>,
        work: &mut Work<'_>,
    ) -> Result<(), ComputeError> {
        paths.accumulate(adjacency, work)?;
        let source = paths.source() as usize;
        let (reached, gained) = paths.reached_and_dependency();
        gained[source] = 0.0;
        if self.scale.endpoints {
            for &node in &reached[1..] {
                gained[node as usize] += 1.0;
            }
            gained[source] = (reached.len() - 1) as f64;
        }
        Ok(())
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
                let first = task * NODES_PER_TASK;
                for paths in round {
                    let gained = &paths.dependency()[first..first + totals.len()];
                    totals.iter_mut().zip(gained).for_each(|(total, gained)| *total += gained);
                }
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
        let sampled = match sources.every_live_node() {
            true => &[][..],
            false => sources.nodes(),
        };
        let k = (!sources.every_live_node()).then_some(sampled.len());
        let (source, other) = rescale(live, self.scale, self.directed, k);
        let mut sampled = sampled.iter().peekable();
        for (node, value) in (0..).zip(totals.values_mut()) {
            let is_source = sampled.next_if(|&&s| s == node).is_some();
            *value *= if is_source { source } else { other };
        }
        Ok(totals)
    }
}

/// The factors NetworkX 3.7's `_rescale` multiplies a sampled source's and
/// any other node's betweenness by, for `n` live nodes and `k` sampled
/// sources (`None`: every node a source).
fn rescale(n: u64, scale: BetweennessScale, directed: bool, k: Option<usize>) -> (f64, f64) {
    // Pairs (s, t), s ≠ t, a path through v could join; without
    // endpoints, v is not t.
    let pairs = n as i128 - i128::from(!scale.endpoints);
    if pairs < 2 {
        return (1.0, 1.0);
    }
    let correction = if directed { 1 } else { 2 };
    let k_sources = k.map_or(pairs, |k| k as i128);
    let factor = |sources: i128| match scale.normalized {
        true => 1.0 / (sources * (pairs - 1)) as f64,
        false => pairs as f64 / (sources * correction) as f64,
    };
    match (k, scale.endpoints) {
        (None, _) | (Some(_), true) => (factor(k_sources), factor(k_sources)),
        // A sampled source is never its own path's end, so it was on
        // paths from one source fewer.
        (Some(_), false) => {
            let source = if k_sources > 1 { factor(k_sources - 1) } else { f64::NAN };
            (source, factor(k_sources))
        }
    }
}
