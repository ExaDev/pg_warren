//! Shortest-path centralities (DESIGN.md §6.7, planned library group 2):
//! betweenness, closeness and harmonic centrality, exact (from every live
//! node) or estimated from sampled sources, as NetworkX defines them.
//!
//! Each runs one single-source search per source (breadth-first, or
//! Dijkstra's algorithm when weighted) and adds what the source contributes
//! to each node into the totals. The sources run in rounds, one per compute
//! thread at a time, each in its own scratch (allocated and charged on the
//! caller's thread before the pool takes it); after each round the
//! contributions are added node by node in ascending source order, in
//! parallel over the nodes. So the totals are summed in one order whatever
//! the thread count, and a seed gives the same result, bit for bit, on any
//! pool. A round waits for its slowest source.

mod adjacency;
mod betweenness;
mod closeness;
mod paths;
mod sources;
#[cfg(test)]
#[cfg(not(miri))]
mod tests;

pub use betweenness::{betweenness_centrality, BetweennessScale};
pub use closeness::{closeness_centrality, harmonic_centrality};
pub use sources::Sampling;

use rayon::prelude::*;

use adjacency::Adjacency;
use paths::Paths;
use sources::Sources;

use super::{ComputeError, ComputeMemory, Vector, Workers};
use crate::graph::{DenseId, GraphCsr, RelationSelection, Tombstones};
use crate::interrupts::{Cancellation, Interrupts, Work};

/// The graph whose shortest paths a centrality counts.
#[derive(Debug, Clone, Copy)]
pub struct ShortestPaths<'a, 'g> {
    /// The graph version.
    pub graph: &'g GraphCsr,
    /// The relations whose edges paths follow.
    pub relations: RelationSelection<'a>,
    /// The version's tombstones: tombstoned nodes are never sources.
    pub tombstones: &'a Tombstones,
    /// Follow edges in their direction; else the undirected simple graph.
    pub directed: bool,
    /// Weigh edges (if `None`, when the graph has weights): shortest paths
    /// are then the lightest, else the fewest edges.
    pub weighted: Option<bool>,
}

/// A centrality vector, and how many sources it was computed from.
#[derive(Debug)]
pub struct Centrality {
    /// Each node's centrality, indexed by dense id (tombstoned nodes 0).
    pub values: Vector<f64>,
    /// The number of sources.
    pub sources: u64,
}

/// The nodes one fold task handles; a task checks the cancellation flag
/// first. Small under test, so tests cross task boundaries.
const NODES_PER_TASK: usize = if cfg!(test) { 3 } else { 4096 };

/// What a centrality adds up over the sources.
trait Measure: Sync {
    /// Whether searches count shortest paths and dependencies.
    const COUNTED: bool;
    /// The per-node totals.
    type Totals: Send;

    /// Zeroed totals over `id_space` nodes.
    fn totals(&self, memory: &ComputeMemory, id_space: u64) -> Result<Self::Totals, ComputeError>;

    /// Prepares what a search from `paths`' source leaves for
    /// [`Self::fold`], after the search.
    fn after_search(
        &self,
        _paths: &mut Paths,
        _adjacency: &Adjacency<'_>,
        _work: &mut Work<'_>,
    ) -> Result<(), ComputeError> {
        Ok(())
    }

    /// Adds a round's contributions, in the order of `round`.
    fn fold(
        &self,
        totals: &mut Self::Totals,
        round: &[Paths],
        cancellation: &Cancellation,
    ) -> Result<(), ComputeError>;

    /// The centrality from the totals over `sources`, `live` the number of
    /// live nodes.
    fn finish(
        &self,
        totals: Self::Totals,
        sources: &Sources,
        live: u64,
    ) -> Result<Vector<f64>, ComputeError>;
}

/// Runs `measure` over the sources `sampling` chooses.
fn run<M: Measure, I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    paths: &ShortestPaths<'_, '_>,
    sampling: &Sampling<'_>,
    measure: &M,
) -> Result<Centrality, ComputeError> {
    let tombstones = paths.tombstones;
    let id_space = tombstones.id_space();
    let live = id_space - tombstones.count();
    let matrix = Adjacency::matrix(paths, memory)?;
    let weighted = matrix.is_weighted();
    let sources_at_most = sampling.k.map_or(live, |k| k.min(live)).max(1);
    let slots = (workers.threads() as u64).min(sources_at_most) as usize;
    let mut totals = measure.totals(memory, id_space)?;
    let mut scratch = (0..slots)
        .map(|_| Paths::new(memory, id_space, M::COUNTED, weighted))
        .collect::<Result<Vec<_>, _>>()?;
    // What the pool builds is charged there, where nothing is reclaimed:
    // the adjacency and the sample (the searches' scratch is charged
    // above; only a multi-relation row scratch grows there unbounded).
    memory.reclaim_for(
        Adjacency::bytes(paths, &matrix).saturating_add(Sources::bytes(sampling, tombstones)),
    );
    let sources = workers.run(interrupts, |cancellation| {
        let adjacency = Adjacency::new(paths, matrix, memory, cancellation)?;
        let sources = Sources::choose(memory, cancellation, sampling, tombstones)?;
        for round in sources.nodes().chunks(slots) {
            let slots = &mut scratch[..round.len()];
            search_round(measure, slots, round, &adjacency, cancellation)?;
            measure.fold(&mut totals, slots, cancellation)?;
        }
        Ok(sources)
    })?;
    let mut values = measure.finish(totals, &sources, live)?;
    values.clear_tombstoned(tombstones)?;
    Ok(Centrality { values, sources: sources.nodes().len() as u64 })
}

/// Searches from each source of `round`, one per slot, in parallel.
fn search_round<M: Measure>(
    measure: &M,
    slots: &mut [Paths],
    round: &[DenseId],
    adjacency: &Adjacency<'_>,
    cancellation: &Cancellation,
) -> Result<(), ComputeError> {
    slots.par_iter_mut().zip(round).try_for_each(|(paths, &source)| {
        let mut work = Work::new(cancellation);
        work.check()?;
        paths.search(adjacency, source, &mut work)?;
        measure.after_search(paths, adjacency, &mut work)
    })
}
