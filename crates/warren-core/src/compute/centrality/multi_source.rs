//! Multi-source breadth-first search (Then et al., "The More the Merrier",
//! VLDB 2014): up to [`SourceBatch::WIDTH`] sources searched together on
//! unweighted rows, one bit per source in a per-node word, so one scan of
//! a frontier node's row serves every source that has it in its frontier.
//!
//! The search is level-synchronous and parallel over nodes. A step pushes
//! each frontier node's word along its row into its neighbours' `next`
//! words (an atomic OR, less the sources that have already reached the
//! neighbour), and the thread whose OR sets a neighbour's word from zero
//! lists it; then each listed node takes its `next` word into `seen` and
//! its frontier, and reports how many sources reached it at that
//! distance. An OR does not depend on the order it runs in, and each node
//! is settled by one thread per step, so what is reported is the same for
//! any thread count.
//!
//! The scratch is three words per node (`seen` and `next` side by side,
//! so a row entry reads one cache line, and the frontier) and two lists of
//! up to every node, allocated before the search; a step touches only the
//! nodes listed, and `seen` is cleared after each search.

use std::mem::size_of;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

use rayon::prelude::*;

use super::super::product::RowScratch;
use super::super::{ComputeError, ComputeMemory, MemoryCharge};
use super::adjacency::Adjacency;
use super::NODES_PER_TASK;
use crate::graph::DenseId;
use crate::interrupts::{Cancellation, Cancelled, Work};

/// A batch search's scratch.
pub(super) struct SourceBatch {
    /// Each node's sources that have reached it, and those that reach it
    /// in the step being taken.
    words: Vec<Words>,
    /// Each node's sources whose frontier it is in.
    frontier: Vec<AtomicU64>,
    /// The frontier's nodes.
    current: Vec<AtomicU64>,
    /// The nodes the step reaches: the next frontier's.
    next: Vec<AtomicU64>,
    _charge: MemoryCharge,
}

/// A node's `seen` and `next` words.
#[derive(Default)]
struct Words {
    seen: AtomicU64,
    next: AtomicU64,
}

/// The nodes a task lists before it claims room for them in the shared
/// list.
const LISTED_PER_CLAIM: usize = 256;

impl SourceBatch {
    /// The most sources one search takes: one per bit of a word.
    pub(super) const WIDTH: usize = u64::BITS as usize;

    /// The bytes [`Self::new`] charges for an id space.
    pub(super) fn bytes(id_space: u64) -> u64 {
        let per_node = size_of::<Words>() + 3 * size_of::<AtomicU64>();
        id_space.saturating_mul(per_node as u64)
    }

    /// An empty scratch over `id_space` nodes.
    pub(super) fn new(memory: &ComputeMemory, id_space: u64) -> Result<Self, ComputeError> {
        let charge = memory.charge(Self::bytes(id_space))?;
        let bytes = charge.bytes();
        fn zeroed<T: Default>(len: u64, bytes: u64) -> Result<Vec<T>, ComputeError> {
            let mut buffer = Vec::new();
            buffer
                .try_reserve_exact(len as usize)
                .map_err(|_| ComputeError::OutOfMemory { bytes })?;
            buffer.resize_with(len as usize, T::default);
            Ok(buffer)
        }
        Ok(Self {
            words: zeroed(id_space, bytes)?,
            frontier: zeroed(id_space, bytes)?,
            current: zeroed(id_space, bytes)?,
            next: zeroed(id_space, bytes)?,
            _charge: charge,
        })
    }

    /// Searches from `sources` (distinct, at most [`Self::WIDTH`]) over
    /// `adjacency`, calling `reached(node, sources, distance)` once for
    /// each node and distance at which `sources` of them first reach it.
    pub(super) fn search(
        &mut self,
        adjacency: &Adjacency<'_>,
        sources: &[DenseId],
        memory: &ComputeMemory,
        cancellation: &Cancellation,
        reached: &(impl Fn(DenseId, u32, u32) + Sync),
    ) -> Result<(), ComputeError> {
        for (bit, &source) in sources.iter().enumerate() {
            let word = 1u64 << bit;
            self.words[source as usize].seen.store(word, Relaxed);
            self.frontier[source as usize].store(word, Relaxed);
            self.current[bit].store(source, Relaxed);
        }
        let mut frontier = sources.len();
        let mut distance = 0;
        while frontier > 0 {
            distance += 1;
            let listed = self.step(adjacency, frontier, memory, cancellation)?;
            self.settle(listed, distance, cancellation, reached)?;
            std::mem::swap(&mut self.current, &mut self.next);
            frontier = listed;
        }
        self.clear(cancellation)
    }

    /// Pushes the frontier's words along their rows into `next`, and lists
    /// the nodes they reach; returns how many.
    fn step(
        &self,
        adjacency: &Adjacency<'_>,
        frontier: usize,
        memory: &ComputeMemory,
        cancellation: &Cancellation,
    ) -> Result<usize, ComputeError> {
        let listed = AtomicUsize::new(0);
        self.current[..frontier].par_chunks(NODES_PER_TASK).try_for_each_init(
            || RowScratch::new(memory),
            |scratch, nodes| {
                let scratch = scratch.as_mut().map_err(|error| error.clone())?;
                let mut work = Work::new(cancellation);
                work.check()?;
                let mut list = List { nodes: [0; LISTED_PER_CLAIM], len: 0 };
                for node in nodes {
                    let node = node.load(Relaxed);
                    work.spend(1)?;
                    let word = self.frontier[node as usize].swap(0, Relaxed);
                    adjacency.for_each(node, scratch, &mut work, |target, _| {
                        let words = &self.words[target as usize];
                        let new = word & !words.seen.load(Relaxed);
                        if new & !words.next.load(Relaxed) != 0
                            && words.next.fetch_or(new, Relaxed) == 0
                        {
                            list.push(target, &self.next, &listed);
                        }
                        Ok(())
                    })?;
                }
                list.claim(&self.next, &listed);
                Ok::<_, ComputeError>(())
            },
        )?;
        Ok(listed.into_inner())
    }

    /// Takes each listed node's `next` word into `seen` and its frontier,
    /// reporting how many sources reached it at `distance`.
    fn settle(
        &self,
        listed: usize,
        distance: u32,
        cancellation: &Cancellation,
        reached: &(impl Fn(DenseId, u32, u32) + Sync),
    ) -> Result<(), Cancelled> {
        self.next[..listed].par_chunks(NODES_PER_TASK).try_for_each(|nodes| {
            if cancellation.is_cancelled() {
                return Err(Cancelled);
            }
            for node in nodes {
                let node = node.load(Relaxed);
                let words = &self.words[node as usize];
                // This thread alone settles the node: a load and a store
                // suffice.
                let word = words.next.swap(0, Relaxed);
                words.seen.store(words.seen.load(Relaxed) | word, Relaxed);
                self.frontier[node as usize].store(word, Relaxed);
                reached(node, word.count_ones(), distance);
            }
            Ok(())
        })
    }

    /// Forgets which sources reached each node.
    fn clear(&self, cancellation: &Cancellation) -> Result<(), ComputeError> {
        Ok(self.words.par_chunks(NODES_PER_TASK).try_for_each(|words| {
            if cancellation.is_cancelled() {
                return Err(Cancelled);
            }
            words.iter().for_each(|words| words.seen.store(0, Relaxed));
            Ok(())
        })?)
    }
}

/// The nodes a task has reached and not yet added to the shared list.
struct List {
    nodes: [DenseId; LISTED_PER_CLAIM],
    len: usize,
}

impl List {
    /// Lists `node`, claiming room in `shared` when full.
    fn push(&mut self, node: DenseId, shared: &[AtomicU64], listed: &AtomicUsize) {
        if self.len == LISTED_PER_CLAIM {
            self.claim(shared, listed);
        }
        self.nodes[self.len] = node;
        self.len += 1;
    }

    /// Adds the nodes to `shared` after the `listed` already there. Each
    /// node is listed once a step, so `shared` (one slot per node) has room.
    fn claim(&mut self, shared: &[AtomicU64], listed: &AtomicUsize) {
        let first = listed.fetch_add(self.len, Relaxed);
        for (slot, &node) in shared[first..first + self.len].iter().zip(&self.nodes[..self.len]) {
            slot.store(node, Relaxed);
        }
        self.len = 0;
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use proptest::prelude::*;

    use super::super::paths::Paths;
    use super::super::ShortestPaths;
    use super::*;
    use crate::compute::reference::{self, Edges};
    use crate::graph::{CsrLayout, RelationSelection, Tombstones};
    use crate::interrupts::Work;

    /// Each `(node, distance)` and how many sources first reach the node
    /// there, other than the node itself.
    type Reports = BTreeMap<(DenseId, u32), u32>;

    /// What one source at a time reports, by [`Paths`].
    fn one_by_one(adjacency: &Adjacency<'_>, id_space: u64, sources: &[DenseId]) -> Reports {
        let memory = ComputeMemory::new(1 << 24);
        let cancellation = Cancellation::new();
        let mut paths = Paths::new(&memory, id_space, false, false).unwrap();
        let mut reports = Reports::new();
        for &source in sources {
            paths.search(adjacency, source, &mut Work::new(&cancellation)).unwrap();
            for (node, &distance) in (0..).zip(paths.distance()) {
                if distance.is_finite() && node != source {
                    *reports.entry((node, distance as u32)).or_default() += 1;
                }
            }
        }
        reports
    }

    /// What batches of [`SourceBatch::WIDTH`] report, in one scratch.
    fn batched(adjacency: &Adjacency<'_>, id_space: u64, sources: &[DenseId]) -> Reports {
        let memory = ComputeMemory::new(1 << 24);
        let cancellation = Cancellation::new();
        let mut batch = SourceBatch::new(&memory, id_space).unwrap();
        let mut reports = Reports::new();
        for batch_sources in sources.chunks(SourceBatch::WIDTH) {
            let each = Mutex::new(Reports::new());
            let reached = |node, sources, distance| {
                assert!(sources > 0, "{node} reported reached by no source");
                let previous = each.lock().unwrap().insert((node, distance), sources);
                assert_eq!(previous, None, "{node} reported twice at {distance}");
            };
            batch.search(adjacency, batch_sources, &memory, &cancellation, &reached).unwrap();
            for (key, count) in each.into_inner().unwrap() {
                *reports.entry(key).or_default() += count;
            }
        }
        reports
    }

    #[test]
    fn an_undirected_edge_is_followed_against_its_direction() {
        // Shrunk from a proptest failure: 18 -> 0 in a 20-node id space,
        // searched undirected from 0.
        let graph = reference::graph(20, 1, &vec![(0, 18, 0, 1.0)], false, false);
        let tombstones = Tombstones::new(20).unwrap();
        let paths = ShortestPaths {
            graph: &graph,
            relations: RelationSelection::All,
            tombstones: &tombstones,
            directed: false,
            weighted: None,
        };
        let memory = ComputeMemory::new(1 << 24);
        let matrix = Adjacency::matrix(&paths, &memory).unwrap();
        let adjacency = Adjacency::new(&paths, matrix, &memory, &Cancellation::new()).unwrap();
        let expected = Reports::from([((18, 1), 1)]);
        assert_eq!(one_by_one(&adjacency, 20, &[0]), expected);
        assert_eq!(batched(&adjacency, 20, &[0]), expected);
    }

    #[test]
    fn a_cancelled_search_stops() {
        let edges: Edges = (0..9).map(|n| (0, n, n + 1, 1.0)).collect();
        let graph = reference::graph(10, 1, &edges, false, false);
        let tombstones = Tombstones::new(10).unwrap();
        let paths = ShortestPaths {
            graph: &graph,
            relations: RelationSelection::All,
            tombstones: &tombstones,
            directed: true,
            weighted: None,
        };
        let memory = ComputeMemory::new(1 << 20);
        let cancellation = Cancellation::new();
        let matrix = Adjacency::matrix(&paths, &memory).unwrap();
        let adjacency = Adjacency::new(&paths, matrix, &memory, &cancellation).unwrap();
        let mut batch = SourceBatch::new(&memory, 10).unwrap();
        cancellation.cancel();
        let searched = batch.search(&adjacency, &[0, 3], &memory, &cancellation, &|_, _, _| ());
        assert_eq!(searched, Err(ComputeError::Cancelled));
    }

    proptest! {
        #[test]
        fn a_batch_reaches_what_its_sources_reach_one_by_one(
            id_space in 1u64..300,
            relations in 1usize..3,
            edges in prop::collection::vec((0usize..2, 0u64..300, 0u64..300), 0..900),
            sources in prop::collection::btree_set(0u64..300, 0..300),
            only in prop::option::of(0usize..2),
            directed in any::<bool>(),
            store_merged in any::<bool>(),
        ) {
            let edges: Edges = edges
                .into_iter()
                .filter(|&(r, s, t)| r < relations && s < id_space && t < id_space)
                .map(|(r, s, t)| (r, s, t, 1.0))
                .collect();
            let layout = CsrLayout { build_reverse: false, store_merged };
            let graph = reference::graph_as(id_space, relations, &edges, false, layout);
            let tombstones = Tombstones::new(id_space).unwrap();
            let sources: Vec<DenseId> = sources.into_iter().filter(|&s| s < id_space).collect();
            let indexes = reference::relations(relations);
            let relations = match only {
                Some(only) => RelationSelection::Only(&indexes[only.min(indexes.len() - 1)..][..1]),
                None => RelationSelection::All,
            };
            let paths = ShortestPaths {
                graph: &graph,
                relations,
                tombstones: &tombstones,
                directed,
                weighted: None,
            };
            let memory = ComputeMemory::new(1 << 24);
            let matrix = Adjacency::matrix(&paths, &memory).unwrap();
            let adjacency = Adjacency::new(&paths, matrix, &memory, &Cancellation::new()).unwrap();
            let expected = one_by_one(&adjacency, id_space, &sources);
            prop_assert_eq!(batched(&adjacency, id_space, &sources), expected);
        }
    }
}
