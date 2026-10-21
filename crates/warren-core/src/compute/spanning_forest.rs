//! Minimum spanning forests (DESIGN.md §6.7,
//! `warren.minimum_spanning_forest`): a minimum spanning tree of every
//! connected component of the undirected graph underlying the selected
//! relations.
//!
//! Directions are ignored and self-loops dropped. An edge's weight is
//! `A[u][v]`, its weights summed over the selected relations (1 when
//! unweighted), as products read it (§6.3); where both `u → v` and
//! `v → u` exist they are parallel undirected edges, and the lighter one
//! can be in the forest. Edges are ordered by weight in SQL order (`NaN`
//! above every number, `-0 = 0`), then by their smaller and larger
//! endpoints' dense ids: a strict total order, so the forest is unique and
//! does not depend on the algorithm (Kruskal's gives the same edges).
//!
//! Borůvka's algorithm: each round scans every stored edge once and picks
//! each component's lightest edge out of it, then joins the components
//! along those edges in a union–find, so the components at least halve in
//! number each round and at most `log₂ N` rounds run. It reads the forward
//! CSRs only, on one thread of the pool, checking its cancellation flag
//! every 65,536 nodes and entries; besides the forest it keeps a parent
//! and a candidate edge per node, charged and reclaimed for ahead.

use std::cmp::Ordering;
use std::mem::size_of;

use super::memory::MemoryCharge;
use super::product::Matrix;
use super::{ComputeError, ComputeMemory, Workers};
use crate::graph::{DenseId, GraphCsr, RelationSelection};
use crate::interrupts::{Cancellation, Interrupts, Work};

/// A forest edge: the stored edge `source → target` and its weight.
pub type ForestEdge = (DenseId, DenseId, f64);

/// The edges of a minimum spanning forest, ascending by source and then
/// target, charged to the budget for as long as they are held.
#[derive(Debug)]
pub struct SpanningForest {
    /// The forest's edges.
    pub edges: Vec<ForestEdge>,
    _charge: MemoryCharge,
}

/// The minimum spanning forest of the undirected graph under the selected
/// relations: weighted if `weighted` (if `None`, when the graph has
/// weights), else every edge weighs 1.
pub fn minimum_spanning_forest<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &GraphCsr,
    relations: RelationSelection<'_>,
    weighted: Option<bool>,
) -> Result<SpanningForest, ComputeError> {
    let matrix = Matrix::new(graph, relations, false, weighted, memory)?;
    let id_space = graph.all_relations().forward().id_space();
    // A parent and a candidate per node, and the forest's buffer, which
    // grows by doubling to at most twice its N − 1 edges.
    let per_node = size_of::<DenseId>() + size_of::<Candidate>() + 2 * size_of::<ForestEdge>();
    memory.reclaim_for(id_space.saturating_mul(per_node as u64));
    workers.run(interrupts, |cancellation| {
        Boruvka::new(memory, cancellation, &matrix, id_space)?.run()
    })
}

/// A component's lightest edge out of it found so far, if any.
type Candidate = Option<ForestEdge>;

/// The order of forest candidates: weight in SQL order, then the smaller
/// and the larger endpoint.
fn lighter(a: &ForestEdge, b: &ForestEdge) -> bool {
    let key = |&(source, target, weight): &ForestEdge| {
        // NaN is canonical (and so above every number), and −0 is 0.
        let weight = if weight.is_nan() { f64::NAN } else { weight + 0.0 };
        (weight, source.min(target), source.max(target))
    };
    let ((wa, sa, ta), (wb, sb, tb)) = (key(a), key(b));
    wa.total_cmp(&wb).then(sa.cmp(&sb)).then(ta.cmp(&tb)) == Ordering::Less
}

struct Boruvka<'a, 'g> {
    matrix: &'a Matrix<'g>,
    cancellation: &'a Cancellation,
    /// Each node's parent in the union–find; roots are their own.
    parents: Vec<DenseId>,
    /// Each root's lightest edge out of its component this round.
    lightest: Vec<Candidate>,
    forest: Vec<ForestEdge>,
    forest_charge: MemoryCharge,
    _charges: [MemoryCharge; 2],
}

impl<'a, 'g> Boruvka<'a, 'g> {
    fn new(
        memory: &ComputeMemory,
        cancellation: &'a Cancellation,
        matrix: &'a Matrix<'g>,
        id_space: u64,
    ) -> Result<Self, ComputeError> {
        let (mut parents, parents_charge) = memory.filled(id_space as usize, 0)?;
        for (node, parent) in (0..).zip(parents.iter_mut()) {
            *parent = node;
        }
        let (lightest, lightest_charge) = memory.filled(id_space as usize, None)?;
        Ok(Self {
            matrix,
            cancellation,
            parents,
            lightest,
            forest: Vec::new(),
            forest_charge: memory.charge(0)?,
            _charges: [parents_charge, lightest_charge],
        })
    }

    fn run(mut self) -> Result<SpanningForest, ComputeError> {
        let mut work = Work::new(self.cancellation);
        while self.round(&mut work)? {}
        self.forest.sort_unstable_by_key(|&(source, target, _)| (source, target));
        Ok(SpanningForest { edges: self.forest, _charge: self.forest_charge })
    }

    /// One round: finds each component's lightest edge out and joins along
    /// them; whether any component was joined.
    fn round(&mut self, work: &mut Work<'_>) -> Result<bool, ComputeError> {
        let nodes = self.parents.len() as DenseId;
        for node in 0..nodes {
            work.spend(1)?;
            self.parents[node as usize] = root(&mut self.parents, node);
            self.lightest[node as usize] = None;
        }
        let mut scratch = self.matrix.scratch()?;
        for source in 0..nodes {
            work.spend(1)?;
            let (parents, lightest) = (&self.parents, &mut self.lightest);
            self.matrix.for_each_entry(source, &mut scratch, |target, weight| {
                let (from, to) = (parents[source as usize], parents[target as usize]);
                if from != to {
                    let edge = (source, target, weight);
                    for component in [from, to] {
                        let best = &mut lightest[component as usize];
                        if best.is_none_or(|best| lighter(&edge, &best)) {
                            *best = Some(edge);
                        }
                    }
                }
                Ok(work.spend(1)?)
            })?;
        }
        let mut joined = false;
        for component in 0..nodes as usize {
            work.spend(1)?;
            let Some(edge @ (source, target, _)) = self.lightest[component] else { continue };
            let (a, b) = (root(&mut self.parents, source), root(&mut self.parents, target));
            if a != b {
                self.forest_charge.reserve_one(&mut self.forest)?;
                self.forest.push(edge);
                self.parents[a.max(b) as usize] = a.min(b);
                joined = true;
            }
        }
        Ok(joined)
    }
}

/// The root of `node`'s set, halving the path on the way.
fn root(parents: &mut [DenseId], mut node: DenseId) -> DenseId {
    while parents[node as usize] != node {
        let grandparent = parents[parents[node as usize] as usize];
        parents[node as usize] = grandparent;
        node = grandparent;
    }
    node
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

    fn forest(
        graph: &GraphCsr,
        relations: RelationSelection<'_>,
        weighted: Option<bool>,
    ) -> Result<Vec<ForestEdge>, ComputeError> {
        let memory = ComputeMemory::new(1 << 28);
        let found = minimum_spanning_forest(
            workers(),
            &memory,
            &mut Uninterrupted,
            graph,
            relations,
            weighted,
        )?;
        Ok(found.edges)
    }

    #[test]
    fn each_component_gets_its_lightest_tree() {
        // Square 0 – 1 – 2 – 3 – 0 with weights 1, 4, 2, 3 and diagonal
        // 0 → 2 of 5; 4 – 5 of 7 (both ways, 7 and 9); 6 alone, 6 → 6.
        let edges: Edges = vec![
            (0, 0, 1, 1.0),
            (0, 2, 1, 4.0),
            (0, 2, 3, 2.0),
            (0, 3, 0, 3.0),
            (0, 0, 2, 5.0),
            (0, 4, 5, 9.0),
            (0, 5, 4, 7.0),
            (0, 6, 6, 0.0),
        ];
        let graph = reference::graph(7, 1, &edges, true, false);
        let all = RelationSelection::All;
        let expected = vec![(0, 1, 1.0), (2, 3, 2.0), (3, 0, 3.0), (5, 4, 7.0)];
        assert_eq!(forest(&graph, all, None), Ok(expected));
        let unweighted = forest(&graph, all, Some(false)).unwrap();
        assert_eq!(unweighted.len(), 4);
        assert!(unweighted.iter().all(|&(_, _, weight)| weight == 1.0));
    }

    #[test]
    fn weights_order_as_sql_orders_them() {
        // NaN is heaviest; −0 ties 0, and the smaller endpoints win.
        let edges: Edges =
            vec![(0, 0, 1, f64::NAN), (0, 1, 2, -0.0), (0, 0, 2, 0.0), (0, 2, 3, f64::INFINITY)];
        let graph = reference::graph(4, 1, &edges, true, false);
        let all = RelationSelection::All;
        let found = forest(&graph, all, None).unwrap();
        let pairs: Vec<_> = found.iter().map(|&(s, t, _)| (s, t)).collect();
        assert_eq!(pairs, [(0, 2), (1, 2), (2, 3)]);
    }

    #[test]
    fn an_interrupt_cancels_the_forest() {
        struct Stop;
        impl Interrupts for Stop {
            fn check(&mut self) -> Result<(), Cancelled> {
                Err(Cancelled)
            }
        }
        let graph = reference::graph(3, 1, &vec![(0, 0, 1, 1.0)], false, false);
        let memory = ComputeMemory::new(1 << 20);
        let all = RelationSelection::All;
        let result = minimum_spanning_forest(workers(), &memory, &mut Stop, &graph, all, None);
        assert_eq!(result.map(|_| ()), Err(ComputeError::Cancelled));
    }

    #[test]
    fn a_cancellation_stops_a_round() {
        let n = 200_000;
        let path: Edges = (0..n - 1).map(|i| (0, i, i + 1, 1.0)).collect();
        let graph = reference::graph(n, 1, &path, false, false);
        let memory = ComputeMemory::new(1 << 28);
        let matrix = Matrix::new(&graph, RelationSelection::All, false, None, &memory).unwrap();
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let boruvka = Boruvka::new(&memory, &cancellation, &matrix, n).unwrap();
        assert_eq!(boruvka.run().map(|_| ()), Err(ComputeError::Cancelled));
    }

    #[test]
    fn the_scratch_and_the_forest_are_charged_and_released() {
        let path: Edges = (0..9).map(|i| (0, i, i + 1, 1.0)).collect();
        let graph = reference::graph(10, 1, &path, false, false);
        let fits = |limit| {
            let memory = ComputeMemory::new(limit);
            let all = RelationSelection::All;
            let found =
                minimum_spanning_forest(workers(), &memory, &mut Uninterrupted, &graph, all, None);
            let fitted = found.is_ok();
            drop(found);
            assert_eq!(memory.in_use(), 0, "every charge is released");
            fitted
        };
        // Parents (80 bytes) and candidates (10 × 32), and the forest's
        // 9 edges (24 bytes each, in a buffer that grows by doubling).
        let scratch = 80 + 10 * size_of::<Candidate>() as u64;
        assert!(!fits(scratch + 9 * 24 - 1));
        assert!(fits(scratch + 16 * 24));
    }

    #[test]
    fn held_memory_is_reclaimed_before_the_pool_charges() {
        let graph = reference::graph(3, 1, &vec![(0, 2, 0, 1.0)], false, false);
        let held = 1 << 20;
        let memory = ComputeMemory::reclaiming(held, held, held, Box::new(Exact));
        let all = RelationSelection::All;
        let found =
            minimum_spanning_forest(workers(), &memory, &mut Uninterrupted, &graph, all, None);
        assert_eq!(found.map(|found| found.edges), Ok(vec![(2, 0, 1.0)]));
    }

    /// Kruskal's forest over `A`'s entries in both directions, self-loops
    /// dropped, as `(smaller, larger, weight)` sorted.
    fn kruskal(id_space: u64, adjacency: &[Vec<Option<f64>>]) -> Vec<(u64, u64, f64)> {
        let mut entries: Vec<ForestEdge> = Vec::new();
        for (u, row) in adjacency.iter().enumerate() {
            for (v, weight) in row.iter().enumerate() {
                if let (Some(weight), true) = (weight, u != v) {
                    entries.push((u as u64, v as u64, *weight));
                }
            }
        }
        entries.sort_by(|a, b| match (lighter(a, b), lighter(b, a)) {
            (true, _) => Ordering::Less,
            (_, true) => Ordering::Greater,
            _ => Ordering::Equal,
        });
        let mut parents: Vec<u64> = (0..id_space).collect();
        let mut chosen = Vec::new();
        for (u, v, weight) in entries {
            let (a, b) = (root(&mut parents, u), root(&mut parents, v));
            if a != b {
                parents[a.max(b) as usize] = a.min(b);
                chosen.push((u.min(v), u.max(v), weight));
            }
        }
        chosen.sort_by_key(|&(u, v, _)| (u, v));
        chosen
    }

    type Scenario = (u64, usize, Edges, Option<Vec<usize>>, (bool, bool));

    fn scenarios() -> impl Strategy<Value = Scenario> {
        (1u64..12, 1usize..4).prop_flat_map(|(id_space, relations)| {
            let weight = (-3i32..6).prop_map(f64::from);
            let edge = (0..relations, 0..id_space, 0..id_space, weight);
            (
                Just(id_space),
                Just(relations),
                prop::collection::vec(edge, 0..40),
                prop::option::of(prop::collection::vec(0..relations, 0..2)),
                any::<(bool, bool)>(),
            )
        })
    }

    proptest! {
        #[test]
        fn the_forest_is_kruskals((id_space, relations, edges, selected, (wide, store_merged)) in scenarios()) {
            let indexes = reference::relations(relations);
            let mut list = EdgeList::new(true);
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
            let adjacency = reference::adjacency(id_space, &edges, selected.as_deref(), true);
            let found = forest(&graph, selection, None).unwrap();
            let mut pairs: Vec<_> = found.iter().map(|&(s, t, w)| (s.min(t), s.max(t), w)).collect();
            pairs.sort_by_key(|&(u, v, _)| (u, v));
            prop_assert_eq!(pairs, kruskal(id_space, &adjacency));
            let sorted = found.windows(2).all(|pair| (pair[0].0, pair[0].1) < (pair[1].0, pair[1].1));
            prop_assert!(sorted);
        }
    }
}
