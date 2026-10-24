//! Directed acyclic graphs (DESIGN.md §6.7): `warren.topological_order`
//! and `warren.dag_longest_path`.
//!
//! Both run Kahn's algorithm over the forward rows of the selected
//! relations (no reverse CSR is needed), an edge in several relations
//! being one entry whose weight is their sum, as products read `A[i][j]`
//! (§6.3). Ready nodes are taken smallest dense id first, so the order is
//! the lexicographically smallest topological order by dense id
//! (NetworkX's `lexicographical_topological_sort`). A graph with a cycle
//! (a self-loop included) has none: [`ComputeError::Cycle`]. It runs on
//! one thread of the pool, `O((N + M) log N)`, checking its cancellation
//! flag every 65,536 nodes and entries; the in-degrees and the ready heap
//! are charged before the pool takes them.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::mem::size_of;

use super::product::{Matrix, Reading};
use super::{ComputeError, ComputeMemory, MemoryCharge, Vector, Workers};
use crate::graph::{DenseId, GraphCsr, RelationSelection, Tombstones};
use crate::interrupts::{Cancellation, Interrupts, Work};

/// Each live node's position in the topological order of the selected
/// relations' edges (from 0; tombstoned nodes hold 0): a node comes after
/// every node with an edge to it, and among the nodes ready at once the
/// smallest dense id comes first.
pub fn topological_order<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &GraphCsr,
    relations: RelationSelection<'_>,
    tombstones: &Tombstones,
) -> Result<Vector<i64>, ComputeError> {
    let matrix = Matrix::new(graph, relations, false, Some(false), false, memory)?;
    let id_space = graph.all_relations().forward().id_space();
    let mut positions = Vector::filled(memory, id_space, 1, 0i64)?;
    let mut order = Positions { positions: positions.values_mut()?, next: 0 };
    memory.reclaim_for(Kahn::bytes(id_space));
    workers.run(interrupts, |cancellation| {
        Kahn::new(memory, cancellation, &matrix, tombstones)?.run(&mut order)
    })?;
    Ok(positions)
}

/// The lengths of the longest paths into each node, and of the longest.
#[derive(Debug, PartialEq)]
pub struct LongestPaths {
    /// Each node's longest path ending there, indexed by dense id: at least
    /// 0, the empty path's length (tombstoned nodes hold 0).
    pub lengths: Vector<f64>,
    /// The longest of them: NetworkX's `dag_longest_path_length` (0 with
    /// no nodes).
    pub longest: f64,
}

/// The length of the longest path ending at each live node, over the
/// selected relations' edges in the topological order: the sum of its
/// edges' weights if `reading` is weighted (if `None`, when the graph has
/// weights), else its number of edges. Per relation, a pair in several
/// relations is a parallel edge in each, so the heaviest lengthens a path.
/// As NetworkX's `dag_longest_path`, a path never starts before a prefix
/// of negative length: lengths are at least 0. A NaN weight never
/// lengthens a path.
pub fn dag_longest_path<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &GraphCsr,
    relations: RelationSelection<'_>,
    reading: Reading,
    tombstones: &Tombstones,
) -> Result<LongestPaths, ComputeError> {
    let matrix =
        Matrix::new(graph, relations, false, reading.weighted, reading.per_relation, memory)?;
    let id_space = graph.all_relations().forward().id_space();
    let mut lengths = Vector::filled(memory, id_space, 1, 0f64)?;
    let mut longest = Lengths { lengths: lengths.values_mut()? };
    memory.reclaim_for(Kahn::bytes(id_space));
    workers.run(interrupts, |cancellation| {
        Kahn::new(memory, cancellation, &matrix, tombstones)?.run(&mut longest)
    })?;
    let longest = lengths.values().iter().copied().fold(0.0, f64::max);
    Ok(LongestPaths { lengths, longest })
}

/// What a pass of Kahn's algorithm does with each node it takes and each
/// edge out of it, in topological order.
trait Visit {
    /// `node` is next in the order.
    fn node(&mut self, _node: DenseId) {}

    /// The edge `from → to` of weight `weight`, once `from` is taken.
    fn edge(&mut self, _from: DenseId, _to: DenseId, _weight: f64) {}
}

/// Numbers the nodes in the order they are taken.
struct Positions<'a> {
    positions: &'a mut [i64],
    next: i64,
}

impl Visit for Positions<'_> {
    fn node(&mut self, node: DenseId) {
        self.positions[node as usize] = self.next;
        self.next += 1;
    }
}

/// Extends each node's longest path along its out-edges.
struct Lengths<'a> {
    lengths: &'a mut [f64],
}

impl Visit for Lengths<'_> {
    fn edge(&mut self, from: DenseId, to: DenseId, weight: f64) {
        let through = self.lengths[from as usize] + weight;
        if through > self.lengths[to as usize] {
            self.lengths[to as usize] = through;
        }
    }
}

/// Kahn's algorithm: each node waits for its in-edges; the smallest ready
/// node is taken next.
struct Kahn<'a, 'g> {
    matrix: &'a Matrix<'g>,
    cancellation: &'a Cancellation,
    tombstones: &'a Tombstones,
    /// Each node's in-edges not yet taken.
    waiting: Vec<u64>,
    ready: BinaryHeap<Reverse<DenseId>>,
    _charges: [MemoryCharge; 2],
}

impl<'a, 'g> Kahn<'a, 'g> {
    /// The bytes [`Self::new`] charges for an id space: the in-degrees and
    /// the heap at its largest.
    fn bytes(id_space: u64) -> u64 {
        id_space.saturating_mul((size_of::<u64>() + size_of::<DenseId>()) as u64)
    }

    fn new(
        memory: &'a ComputeMemory,
        cancellation: &'a Cancellation,
        matrix: &'a Matrix<'g>,
        tombstones: &'a Tombstones,
    ) -> Result<Self, ComputeError> {
        let id_space = tombstones.id_space();
        let (waiting, waiting_charge) = memory.filled(id_space as usize, 0u64)?;
        let heap_charge = memory.charge_elements::<DenseId>(id_space as usize)?;
        let mut ready = BinaryHeap::new();
        ready
            .try_reserve_exact(id_space as usize)
            .map_err(|_| ComputeError::OutOfMemory { bytes: heap_charge.bytes() })?;
        Ok(Self {
            matrix,
            cancellation,
            tombstones,
            waiting,
            ready,
            _charges: [waiting_charge, heap_charge],
        })
    }

    /// Takes every node in order, or fails with the number of live nodes
    /// left waiting on a cycle.
    fn run(mut self, visit: &mut impl Visit) -> Result<(), ComputeError> {
        let mut work = Work::new(self.cancellation);
        let mut scratch = self.matrix.scratch()?;
        let nodes = self.waiting.len() as DenseId;
        for node in 0..nodes {
            work.spend(1)?;
            let waiting = &mut self.waiting;
            self.matrix.for_each_entry(node, &mut scratch, |to, _| {
                waiting[to as usize] += 1;
                Ok(work.spend(1)?)
            })?;
        }
        for node in 0..nodes {
            work.spend(1)?;
            if self.waiting[node as usize] == 0 && !self.tombstones.contains(node) {
                self.ready.push(Reverse(node));
            }
        }
        let mut taken = 0;
        while let Some(Reverse(node)) = self.ready.pop() {
            work.spend(1)?;
            taken += 1;
            visit.node(node);
            let (waiting, ready) = (&mut self.waiting, &mut self.ready);
            self.matrix.for_each_entry(node, &mut scratch, |to, weight| {
                visit.edge(node, to, weight);
                let left = &mut waiting[to as usize];
                *left -= 1;
                if *left == 0 {
                    ready.push(Reverse(to));
                }
                Ok(work.spend(1)?)
            })?;
        }
        match nodes - self.tombstones.count() - taken {
            0 => Ok(()),
            unordered => Err(ComputeError::Cycle { unordered }),
        }
    }
}

// Not under Miri: the compute pool's work stealing runs through
// crossbeam-epoch, which Miri's Stacked Borrows model rejects.
#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::sync::OnceLock;

    use proptest::prelude::*;

    use super::*;
    use crate::compute::memory::Exact;
    use crate::compute::reference::{self, Edges};
    use crate::graph::{CsrLayout, EdgeList};
    use crate::interrupts::{Cancelled, Uninterrupted};

    fn workers() -> &'static Workers {
        static WORKERS: OnceLock<Workers> = OnceLock::new();
        WORKERS.get_or_init(|| Workers::new(3).unwrap())
    }

    fn memory() -> ComputeMemory {
        ComputeMemory::new(1 << 28)
    }

    fn order(graph: &GraphCsr, tombstones: &Tombstones) -> Result<Vec<i64>, ComputeError> {
        let all = RelationSelection::All;
        let found =
            topological_order(workers(), &memory(), &mut Uninterrupted, graph, all, tombstones)?;
        Ok(found.values().to_vec())
    }

    fn longest(
        graph: &GraphCsr,
        relations: RelationSelection<'_>,
        weighted: Option<bool>,
        tombstones: &Tombstones,
    ) -> Result<(Vec<f64>, f64), ComputeError> {
        longest_reading(graph, relations, weighted, false, tombstones)
    }

    fn longest_reading(
        graph: &GraphCsr,
        relations: RelationSelection<'_>,
        weighted: Option<bool>,
        per_relation: bool,
        tombstones: &Tombstones,
    ) -> Result<(Vec<f64>, f64), ComputeError> {
        let reading = Reading { weighted, per_relation };
        let found = dag_longest_path(
            workers(),
            &memory(),
            &mut Uninterrupted,
            graph,
            relations,
            reading,
            tombstones,
        )?;
        Ok((found.lengths.values().to_vec(), found.longest))
    }

    fn none(id_space: u64) -> Tombstones {
        Tombstones::new(id_space).unwrap()
    }

    fn edges(pairs: &[(u64, u64)]) -> Edges {
        pairs.iter().map(|&(s, t)| (0, s, t, 1.0)).collect()
    }

    #[test]
    fn ready_nodes_are_taken_smallest_first() {
        // 4 → 1 → 3, 4 → 0, 2 alone: ready {2, 4}, then {0, 1}, then {3}.
        let graph = reference::graph(5, 1, &edges(&[(4, 1), (1, 3), (4, 0)]), false, false);
        // Order 2, 4, 0, 1, 3.
        assert_eq!(order(&graph, &none(5)), Ok(vec![2, 3, 0, 4, 1]));
    }

    #[test]
    fn tombstoned_nodes_take_no_position() {
        let graph = reference::graph(4, 1, &edges(&[(3, 1)]), false, false);
        let mut tombstones = none(4);
        tombstones.insert(0).unwrap();
        tombstones.insert(2).unwrap();
        assert_eq!(order(&graph, &tombstones), Ok(vec![0, 1, 0, 0]));
    }

    #[test]
    fn a_cycle_or_a_self_loop_has_no_order() {
        let cycle = reference::graph(5, 1, &edges(&[(0, 1), (1, 2), (2, 1), (2, 3)]), false, false);
        assert_eq!(order(&cycle, &none(5)), Err(ComputeError::Cycle { unordered: 3 }));
        let self_loop = reference::graph(2, 1, &edges(&[(1, 1)]), false, false);
        assert_eq!(order(&self_loop, &none(2)), Err(ComputeError::Cycle { unordered: 1 }));
        let all = RelationSelection::All;
        let refused = longest(&self_loop, all, None, &none(2));
        assert_eq!(refused, Err(ComputeError::Cycle { unordered: 1 }));
    }

    #[test]
    fn longest_paths_sum_weights_and_never_go_below_zero() {
        // 0 → 1 (2), 1 → 3 (−5), 0 → 2 (1), 2 → 3 (1), 3 → 4 (4); 5 alone.
        let weighted: Edges =
            vec![(0, 0, 1, 2.0), (0, 1, 3, -5.0), (0, 0, 2, 1.0), (0, 2, 3, 1.0), (0, 3, 4, 4.0)];
        let graph = reference::graph(6, 1, &weighted, true, false);
        let all = RelationSelection::All;
        let expected = vec![0.0, 2.0, 1.0, 2.0, 6.0, 0.0];
        assert_eq!(longest(&graph, all, None, &none(6)), Ok((expected, 6.0)));
        let hops = vec![0.0, 1.0, 1.0, 2.0, 3.0, 0.0];
        assert_eq!(longest(&graph, all, Some(false), &none(6)), Ok((hops, 3.0)));
        let empty = reference::graph(0, 1, &vec![], true, false);
        assert_eq!(longest(&empty, all, None, &none(0)), Ok((vec![], 0.0)));
    }

    #[test]
    fn an_interrupt_cancels_the_order() {
        struct Stop;
        impl Interrupts for Stop {
            fn check(&mut self) -> Result<(), Cancelled> {
                Err(Cancelled)
            }
        }
        let graph = reference::graph(3, 1, &edges(&[(0, 1)]), false, false);
        let (memory, all) = (memory(), RelationSelection::All);
        let result = topological_order(workers(), &memory, &mut Stop, &graph, all, &none(3));
        assert_eq!(result.map(|_| ()), Err(ComputeError::Cancelled));
    }

    #[test]
    fn a_cancellation_stops_a_long_order() {
        let n = 200_000;
        let path: Edges = (0..n - 1).map(|i| (0, i, i + 1, 1.0)).collect();
        let graph = reference::graph(n, 1, &path, false, false);
        let memory = memory();
        let matrix =
            Matrix::new(&graph, RelationSelection::All, false, None, false, &memory).unwrap();
        let (cancellation, tombstones) = (Cancellation::new(), none(n));
        cancellation.cancel();
        let kahn = Kahn::new(&memory, &cancellation, &matrix, &tombstones).unwrap();
        let mut positions = vec![0; n as usize];
        let ran = kahn.run(&mut Positions { positions: &mut positions, next: 0 });
        assert_eq!(ran, Err(ComputeError::Cancelled));
    }

    #[test]
    fn the_in_degrees_and_heap_are_charged_and_released() {
        let graph = reference::graph(10, 1, &edges(&[(0, 1), (1, 2)]), false, false);
        let fits = |limit| {
            let memory = ComputeMemory::new(limit);
            let all = RelationSelection::All;
            let found =
                topological_order(workers(), &memory, &mut Uninterrupted, &graph, all, &none(10));
            let fitted = found.is_ok();
            drop(found);
            assert_eq!(memory.in_use(), 0, "every charge is released");
            fitted
        };
        // The positions, in-degrees and heap: 3 × 80 bytes.
        assert!(fits(240));
        assert!(!fits(239));
    }

    #[test]
    fn held_memory_is_reclaimed_before_the_pool_charges() {
        let graph = reference::graph(3, 1, &edges(&[(2, 0)]), false, false);
        let held = 1 << 20;
        let memory = ComputeMemory::reclaiming(held, held, held, Box::new(Exact));
        let all = RelationSelection::All;
        let found =
            topological_order(workers(), &memory, &mut Uninterrupted, &graph, all, &none(3));
        assert_eq!(found.map(|found| found.values().to_vec()), Ok(vec![2, 0, 1]));
    }

    /// The order the naive way: repeatedly the smallest node with no edge
    /// from a node not yet taken; `None` if some node is never free.
    fn naive_order(id_space: u64, adjacency: &[Vec<Option<f64>>]) -> Option<Vec<i64>> {
        let n = id_space as usize;
        let mut positions = vec![None; n];
        for position in 0..n {
            let free = (0..n).find(|&v| {
                positions[v].is_none()
                    && (0..n).all(|u| positions[u].is_some() || adjacency[u][v].is_none())
            })?;
            positions[free] = Some(position as i64);
        }
        positions.into_iter().collect()
    }

    /// The longest paths the naive way: relax every edge `id_space` times.
    fn naive_longest(id_space: u64, adjacency: &[Vec<Option<f64>>]) -> Vec<f64> {
        let n = id_space as usize;
        let mut lengths = vec![0.0f64; n];
        for _ in 0..n {
            for u in 0..n {
                for v in 0..n {
                    if let Some(weight) = adjacency[u][v] {
                        lengths[v] = lengths[v].max(lengths[u] + weight);
                    }
                }
            }
        }
        lengths
    }

    type Scenario = (u64, usize, Edges, Option<Vec<usize>>, bool, (bool, bool, bool));

    /// Random graphs whose edges mostly run from a smaller rank to a larger
    /// one under a random ranking, so most are acyclic; integer weights,
    /// some negative, so sums are exact.
    fn scenarios() -> impl Strategy<Value = Scenario> {
        (1u64..10, 1usize..4).prop_flat_map(|(id_space, relations)| {
            let weight = (-3i32..6).prop_map(f64::from);
            let edge = (0..relations, 0..id_space, 0..id_space, weight);
            (
                Just(id_space),
                Just(relations),
                prop::collection::vec(edge, 0..30),
                prop::option::of(prop::collection::vec(0..relations, 0..2)),
                prop::bool::weighted(0.8),
                any::<(bool, bool, bool)>(),
            )
        })
    }

    proptest! {
        #[test]
        fn orders_and_longest_paths_match_the_naive_ones(
            (id_space, relations, raw, selected, acyclic, (wide, store_merged, weighted)) in scenarios(),
            per_relation in any::<bool>(),
        ) {
            let edges: Edges = match acyclic {
                true => raw.into_iter().filter(|&(_, s, t, _)| s < t).collect(),
                false => raw,
            };
            let indexes = reference::relations(relations);
            let mut list = EdgeList::new(weighted);
            for &(r, s, t, w) in &edges {
                list.push(indexes[r], s, t, w).unwrap();
            }
            let layout = CsrLayout { build_reverse: false, store_merged };
            let graph = match wide {
                true => GraphCsr::build_with::<u64>(list, relations, id_space, layout),
                false => GraphCsr::build_as(list, relations, id_space, layout),
            }.unwrap();
            let chosen: Option<Vec<_>> = selected.as_ref().map(|s| s.iter().map(|&r| indexes[r]).collect());
            let selection = chosen.as_deref().map_or(RelationSelection::All, RelationSelection::Only);
            let adjacency = reference::adjacency(id_space, &edges, selected.as_deref(), weighted);
            let (memory, tombstones) = (memory(), none(id_space));
            let found = topological_order(workers(), &memory, &mut Uninterrupted, &graph, selection, &tombstones)
                .map(|found| found.values().to_vec());
            let lengths = longest_reading(&graph, selection, None, per_relation, &tombstones);
            // Per relation, a pair's heaviest relation lengthens a path most.
            let read = match per_relation {
                true => reference::adjacency_per_relation(id_space, &edges, selected.as_deref(), weighted, f64::max),
                false => adjacency.clone(),
            };
            match naive_order(id_space, &adjacency) {
                Some(expected) => {
                    prop_assert_eq!(found, Ok(expected));
                    let expected = naive_longest(id_space, &read);
                    let max = expected.iter().copied().fold(0.0, f64::max);
                    prop_assert_eq!(lengths, Ok((expected, max)));
                }
                None => {
                    let refused = matches!(found, Err(ComputeError::Cycle { .. }));
                    prop_assert!(refused);
                    let refused = matches!(lengths, Err(ComputeError::Cycle { .. }));
                    prop_assert!(refused);
                }
            }
        }
    }
}
