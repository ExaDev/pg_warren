//! The most similar nodes to one node, or to each node: candidates found
//! by expanding two hops, `u → w → v`, each `v` gaining one shared
//! neighbour and `1 / ln |N(w)|` per `w`, then ranked by score (ties by
//! dense id) and the best `k` kept.
//!
//! For one node the candidates are gathered in a charged list and sorted;
//! for each node, every compute thread keeps dense per-node tallies (20
//! bytes per node in the id space) and takes the next task of nodes until
//! none are left, so a hub's task does not hold back the rest.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering as Atomic};

use rayon::prelude::*;

use super::super::symmetric::distinct;
use super::super::{ComputeError, ComputeMemory, Mask, MemoryCharge, Workers};
use super::neighbourhoods::{Neighbourhoods, RowBuffer};
use super::{adamic_adar_term, Neighbourhood, Shared, Similarity};
use crate::graph::DenseId;
use crate::interrupts::{Cancellation, Interrupts, Work};

/// The nodes one task ranks for; a task checks the cancellation flag
/// first.
const NODES_PER_TASK: usize = if cfg!(test) { 3 } else { 1024 };

/// A node similar to another, and its score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Similar {
    /// The similar node.
    pub node: DenseId,
    /// Its score.
    pub score: f64,
}

/// Better first: a higher score, then a smaller dense id.
impl Eq for Similar {}

impl PartialOrd for Similar {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Similar {
    fn cmp(&self, other: &Self) -> Ordering {
        other.score.total_cmp(&self.score).then(self.node.cmp(&other.node))
    }
}

/// The `k` best of `candidates`, best first.
#[derive(Debug)]
struct Best {
    k: usize,
    /// The best so far, the worst on top.
    kept: BinaryHeap<Similar>,
    /// The kept candidates, best first, once drained.
    sorted: Vec<Similar>,
    _charge: MemoryCharge,
}

impl Best {
    /// Room for the best `k` of at most `candidates`, charged to `memory`.
    fn new(memory: &ComputeMemory, k: u64, candidates: u64) -> Result<Self, ComputeError> {
        let k = k.min(candidates) as usize;
        let room = k + 1;
        let charge = memory.charge_elements::<Similar>(2 * room)?;
        let out_of_memory = |_| ComputeError::OutOfMemory { bytes: charge.bytes() };
        let mut kept = BinaryHeap::new();
        kept.try_reserve_exact(room).map_err(out_of_memory)?;
        let mut sorted = Vec::new();
        sorted.try_reserve_exact(room).map_err(out_of_memory)?;
        Ok(Self { k, kept, sorted, _charge: charge })
    }

    fn offer(&mut self, candidate: Similar) {
        self.kept.push(candidate);
        if self.kept.len() > self.k {
            self.kept.pop();
        }
    }

    /// The kept candidates, best first, emptying the heap.
    fn drain(&mut self) -> &[Similar] {
        self.sorted.clear();
        self.sorted.extend(self.kept.drain());
        self.sorted.sort_unstable();
        &self.sorted
    }
}

/// Calls `each(v, 1 / ln |N(w)|)` for each two-hop path `u → w → v`,
/// `v ≠ u`, in ascending `w`; `rows.0` then holds `u`'s row.
fn expand<'b>(
    neighbourhoods: &'b Neighbourhoods<'_>,
    node: DenseId,
    rows: (&'b mut RowBuffer, &mut RowBuffer),
    work: &mut Work<'_>,
    mut each: impl FnMut(DenseId, f64) -> Result<(), ComputeError>,
) -> Result<&'b [DenseId], ComputeError> {
    let row = neighbourhoods.row(node, rows.0, work)?;
    for w in distinct(row) {
        let row_w = neighbourhoods.row(w, rows.1, work)?;
        let term = adamic_adar_term(neighbourhoods.degree_of_row(w, row_w));
        for v in distinct(row_w).filter(|&v| v != node) {
            each(v, term)?;
        }
    }
    Ok(row)
}

/// Offers each candidate `v` of `u` (sharing `shared` with it) to `best`:
/// every `v` that is not a neighbour of `u`.
fn rank(
    neighbourhoods: &Neighbourhoods<'_>,
    measure: Similarity,
    (node, row): (DenseId, &[DenseId]),
    candidates: impl Iterator<Item = (DenseId, Shared)>,
    degrees: &mut RowBuffer,
    work: &mut Work<'_>,
    best: &mut Best,
) -> Result<(), ComputeError> {
    let degree = neighbourhoods.degree_of_row(node, row);
    for (v, shared) in candidates {
        if row.binary_search(&v).is_ok() {
            continue;
        }
        let pair = match measure {
            Similarity::Jaccard => (degree, neighbourhoods.degree(v, degrees, work)?),
            _ => (degree, 0),
        };
        best.offer(Similar { node: v, score: measure.score(shared, pair) });
    }
    Ok(())
}

/// The `k` nodes most similar to `node` by `measure`, best first: nodes
/// sharing at least one neighbour with it that are neither it nor its
/// neighbours. Neighbourhoods are read node by node when every selected
/// relation has a reverse CSR, else from the symmetric adjacency, built
/// once on the pool. The ranking itself runs on the calling thread, so
/// what it charges reclaims held memory as it needs it.
pub fn most_similar<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &Neighbourhood<'_, '_>,
    measure: Similarity,
    node: DenseId,
    k: u64,
) -> Result<Vec<Similar>, ComputeError> {
    let neighbourhoods = match Neighbourhoods::local(graph.graph, graph.relations)? {
        Some(local) => local,
        None => {
            memory.reclaim_for(Neighbourhoods::built_bytes(graph.graph, graph.relations)?);
            workers.run(interrupts, |cancellation| {
                Neighbourhoods::built(memory, cancellation, graph.graph, graph.relations)
            })?
        }
    };
    let mut work = Work::polling(interrupts);
    let mut paths = memory.charge(0)?;
    let mut found: Vec<(DenseId, f64)> = Vec::new();
    let (mut row, mut row_w, mut degrees) =
        (RowBuffer::new(memory)?, RowBuffer::new(memory)?, RowBuffer::new(memory)?);
    let row = expand(&neighbourhoods, node, (&mut row, &mut row_w), &mut work, |v, term| {
        paths.reserve_one(&mut found)?;
        found.push((v, term));
        Ok(())
    })?;
    // Stable, so each candidate's terms stay in ascending `w`.
    found.sort_by_key(|&(v, _)| v);
    let runs = found.chunk_by(|a, b| a.0 == b.0);
    let mut best = Best::new(memory, k, runs.clone().count() as u64)?;
    let candidates = runs.map(|run| {
        let adamic_adar = run.iter().map(|&(_, term)| term).sum();
        (run[0].0, Shared { count: run.len() as u64, adamic_adar })
    });
    rank(&neighbourhoods, measure, (node, row), candidates, &mut degrees, &mut work, &mut best)?;
    Ok(best.drain().to_vec())
}

/// Each selected node's most similar nodes, as [`most_similar`] ranks
/// them, charged to the compute budget.
#[derive(Debug)]
pub struct SimilarPairs {
    /// Tasks of nodes in ascending dense id, each the `(node, similar)`
    /// pairs of its nodes, best first per node.
    pub tasks: Vec<Vec<(DenseId, Similar)>>,
    _charges: Vec<MemoryCharge>,
}

/// One compute thread's tallies: each candidate's shared neighbours and
/// Adamic–Adar sum, the candidates of the node in hand, and its best `k`.
struct Tally {
    shared: Vec<u32>,
    adamic_adar: Vec<f64>,
    touched: Vec<DenseId>,
    best: Best,
    _charge: MemoryCharge,
}

impl Tally {
    /// The bytes one tally's arrays charge over `id_space` nodes.
    fn bytes(id_space: u64) -> u64 {
        id_space.saturating_mul((size_of::<u32>() + size_of::<f64>() + size_of::<DenseId>()) as u64)
    }

    fn new(memory: &ComputeMemory, id_space: u64, k: u64) -> Result<Self, ComputeError> {
        let charge = memory.charge(Self::bytes(id_space))?;
        let out_of_memory = |_| ComputeError::OutOfMemory { bytes: charge.bytes() };
        let (mut shared, mut adamic_adar, mut touched) = (Vec::new(), Vec::new(), Vec::new());
        shared.try_reserve_exact(id_space as usize).map_err(out_of_memory)?;
        adamic_adar.try_reserve_exact(id_space as usize).map_err(out_of_memory)?;
        touched.try_reserve_exact(id_space as usize).map_err(out_of_memory)?;
        shared.resize(id_space as usize, 0);
        adamic_adar.resize(id_space as usize, 0.0);
        let best = Best::new(memory, k, id_space)?;
        Ok(Self { shared, adamic_adar, touched, best, _charge: charge })
    }
}

/// A task's index and its pairs.
type Ranked = (usize, Vec<(DenseId, Similar)>);

/// At most the bytes the pool charges for `ranked` nodes' best `k`, in
/// `tasks` tasks: every pair (in buffers that grow by doubling, so at most
/// twice what they hold) and every task's entry.
fn result_bytes(ranked: u64, k: u64, tasks: u64) -> u64 {
    let pairs = ranked.saturating_mul(k).saturating_mul(size_of::<(DenseId, Similar)>() as u64);
    let entries = tasks.saturating_mul(size_of::<Ranked>() as u64);
    pairs.saturating_add(entries).saturating_mul(2)
}

/// Each live node's `k` most similar nodes (only the nodes `nodes`
/// selects, if given), as [`most_similar`] ranks them, from the symmetric
/// adjacency built once. The result is charged as it grows.
pub fn similar_pairs<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &Neighbourhood<'_, '_>,
    measure: Similarity,
    k: u64,
    nodes: Option<Mask<'_>>,
) -> Result<SimilarPairs, ComputeError> {
    let id_space = graph.graph.all_relations().forward().id_space();
    if let Some(nodes) = &nodes {
        nodes.check(id_space)?;
    }
    let selected = |node| nodes.is_none_or(|mask: Mask<'_>| mask.selects(node));
    let ranked = (0..id_space).filter(|&node| selected(node) && !graph.tombstones.contains(node));
    let ranked = ranked.count() as u64;
    let threads = workers.threads().min(ranked.max(1) as usize);
    let mut tallies =
        (0..threads).map(|_| Tally::new(memory, id_space, k)).collect::<Result<Vec<_>, _>>()?;
    let tasks = id_space.div_ceil(NODES_PER_TASK as u64);
    let built = Neighbourhoods::built_bytes(graph.graph, graph.relations)?;
    memory.reclaim_for(built.saturating_add(result_bytes(ranked, k.min(id_space), tasks)));
    let done = workers.run(interrupts, |cancellation| {
        let neighbourhoods =
            Neighbourhoods::built(memory, cancellation, graph.graph, graph.relations)?;
        let next = AtomicUsize::new(0);
        let ranker = Ranker { memory, cancellation, neighbourhoods, measure, nodes, next: &next };
        tallies
            .par_iter_mut()
            .map(|tally| ranker.take_tasks(tally, id_space))
            .collect::<Result<Vec<_>, _>>()
    })?;
    let mut charges = vec![memory.charge_elements::<Ranked>(tasks as usize)?];
    let mut ordered = Vec::new();
    ordered.try_reserve_exact(tasks as usize).map_err(|_| ComputeError::OutOfMemory {
        bytes: tasks.saturating_mul(size_of::<Ranked>() as u64),
    })?;
    for (charge, tasks) in done {
        charges.push(charge);
        ordered.extend(tasks);
    }
    ordered.sort_unstable_by_key(|&(task, _)| task);
    let tasks = ordered.into_iter().map(|(_, pairs)| pairs).collect();
    Ok(SimilarPairs { tasks, _charges: charges })
}

/// What each compute thread ranks with.
struct Ranker<'a, 'g> {
    memory: &'a ComputeMemory,
    cancellation: &'a Cancellation,
    neighbourhoods: Neighbourhoods<'g>,
    measure: Similarity,
    nodes: Option<Mask<'a>>,
    next: &'a AtomicUsize,
}

impl Ranker<'_, '_> {
    /// Takes tasks until none are left, ranking each with `tally`; returns
    /// the tasks' pairs and their charge.
    fn take_tasks(
        &self,
        tally: &mut Tally,
        id_space: u64,
    ) -> Result<(MemoryCharge, Vec<Ranked>), ComputeError> {
        let mut work = Work::new(self.cancellation);
        // Built rows are read in place: these never grow.
        let (mut row, mut row_w, mut degrees) = (
            RowBuffer::new(self.memory)?,
            RowBuffer::new(self.memory)?,
            RowBuffer::new(self.memory)?,
        );
        let mut charge = self.memory.charge(0)?;
        let mut done = Vec::new();
        loop {
            let task = self.next.fetch_add(1, Atomic::Relaxed);
            let first = (task as u64).saturating_mul(NODES_PER_TASK as u64);
            if first >= id_space {
                return Ok((charge, done));
            }
            work.check()?;
            let mut pairs = Vec::new();
            for node in first..id_space.min(first + NODES_PER_TASK as u64) {
                if !self.nodes.is_none_or(|mask| mask.selects(node)) {
                    continue;
                }
                let Tally { shared, adamic_adar, touched, best, .. } = &mut *tally;
                let row = expand(
                    &self.neighbourhoods,
                    node,
                    (&mut row, &mut row_w),
                    &mut work,
                    |v, term| {
                        let at = v as usize;
                        if shared[at] == 0 {
                            touched.push(v);
                        }
                        shared[at] += 1;
                        adamic_adar[at] += term;
                        Ok(())
                    },
                )?;
                let candidates = touched.iter().map(|&v| {
                    let at = v as usize;
                    let found =
                        Shared { count: u64::from(shared[at]), adamic_adar: adamic_adar[at] };
                    (v, found)
                });
                rank(
                    &self.neighbourhoods,
                    self.measure,
                    (node, row),
                    candidates,
                    &mut degrees,
                    &mut work,
                    best,
                )?;
                for &v in touched.iter() {
                    shared[v as usize] = 0;
                    adamic_adar[v as usize] = 0.0;
                }
                touched.clear();
                for &similar in best.drain() {
                    charge.reserve_one(&mut pairs)?;
                    pairs.push((node, similar));
                }
            }
            charge.reserve_one(&mut done)?;
            done.push((task, pairs));
        }
    }
}
