//! Core numbers (DESIGN.md §6.7, `warren.core_number`): each node's
//! k-core number in the undirected simple graph underlying the selected
//! relations, as NetworkX's `core_number` defines it, and the graph's
//! degeneracy, the largest of them.
//!
//! The kernel builds the symmetric adjacency (`compute::symmetric`, charged
//! to the budget), then peels it with Batagelj and Zaversnik's bucket
//! algorithm ("An O(m) Algorithm for Cores Decomposition of Networks",
//! 2003): nodes sorted by degree into buckets, each node taken in turn
//! from the lowest bucket, its neighbours of higher degree moved one
//! bucket down. O(N + M) after the adjacency's sort, on one thread of the
//! pool, checking its cancellation flag every 65,536 row entries.

use std::mem::size_of;

use super::symmetric::{distinct, selected_rows, Symmetric};
use super::{ComputeError, ComputeMemory, Vector, Workers};
use crate::graph::{GraphCsr, RelationSelection};
use crate::interrupts::{Cancellation, Interrupts, Work};

/// Each node's core number, and the graph's degeneracy.
#[derive(Debug, PartialEq)]
pub struct CoreNumbers {
    /// Each node's core number, indexed by dense id (0 for a node with no
    /// neighbours, tombstoned nodes included).
    pub cores: Vector<i64>,
    /// The largest core number: 0 for a graph without edges.
    pub degeneracy: i64,
}

/// Each node's core number over the selected relations, edge directions
/// and self-loops ignored.
pub fn core_number<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &GraphCsr,
    relations: RelationSelection<'_>,
) -> Result<CoreNumbers, ComputeError> {
    let forward = selected_rows(graph, relations)?;
    let id_space = graph.all_relations().forward().id_space();
    let mut cores = Vector::filled(memory, id_space, 1, 0i64)?;
    let degrees = cores.values_mut()?;
    // The adjacency and the peeling's three arrays are charged on the pool,
    // which reclaims no held memory, so room is reclaimed here first.
    let peeling = id_space.saturating_mul(3).saturating_add(1);
    let scratch = peeling.saturating_mul(size_of::<u64>() as u64);
    memory.reclaim_for(Symmetric::bytes(&forward, id_space).saturating_add(scratch));
    let degeneracy = workers.run(interrupts, |cancellation| {
        let adjacency = Symmetric::new(memory, cancellation, &forward, id_space)?;
        peel(memory, cancellation, &adjacency, degrees)
    })?;
    Ok(CoreNumbers { cores, degeneracy })
}

/// Turns `degrees` (scratch on entry) into each node's core number and
/// returns the largest.
fn peel(
    memory: &ComputeMemory,
    cancellation: &Cancellation,
    adjacency: &Symmetric,
    degrees: &mut [i64],
) -> Result<i64, ComputeError> {
    let nodes = degrees.len();
    let mut work = Work::new(cancellation);
    let mut largest = 0;
    for (node, degree) in degrees.iter_mut().enumerate() {
        let row = adjacency.row(node);
        work.spend(row.len() as u64 + 1)?;
        *degree = distinct(row).count() as i64;
        largest = largest.max(*degree as usize);
    }
    // `starts[d]`: where the nodes of degree `d` begin in `order`, which
    // holds the nodes by ascending current degree; `at[v]`: `v`'s place.
    let (mut starts, _starts_charge) = memory.filled(largest + 1, 0usize)?;
    for &degree in degrees.iter() {
        work.spend(1)?;
        starts[degree as usize] += 1;
    }
    let mut start = 0;
    for bucket in starts.iter_mut() {
        work.spend(1)?;
        (*bucket, start) = (start, start + *bucket);
    }
    let (mut order, _order_charge) = memory.filled(nodes, 0usize)?;
    let (mut at, _at_charge) = memory.filled(nodes, 0usize)?;
    for (node, &degree) in degrees.iter().enumerate() {
        work.spend(1)?;
        let bucket = &mut starts[degree as usize];
        (at[node], order[*bucket]) = (*bucket, node);
        *bucket += 1;
    }
    starts.copy_within(..largest, 1);
    starts[0] = 0;
    for next in 0..nodes {
        let node = order[next];
        let row = adjacency.row(node);
        work.spend(row.len() as u64 + 1)?;
        for neighbour in distinct(row) {
            let neighbour = neighbour as usize;
            let degree = degrees[neighbour];
            if degree > degrees[node] {
                // Swap the neighbour with the first node of its bucket,
                // then move the bucket's start past it: one bucket down.
                let first = starts[degree as usize];
                let swapped = order[first];
                (order[at[neighbour]], order[first]) = (swapped, neighbour);
                (at[swapped], at[neighbour]) = (at[neighbour], first);
                starts[degree as usize] += 1;
                degrees[neighbour] -= 1;
            }
        }
    }
    Ok(degrees.iter().copied().max().unwrap_or(0))
}

// Not under Miri: the compute pool's work stealing runs through
// crossbeam-epoch, which Miri's Stacked Borrows model rejects.
#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::collections::BTreeSet;
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

    fn cores(
        graph: &GraphCsr,
        relations: RelationSelection<'_>,
    ) -> Result<(Vec<i64>, i64), ComputeError> {
        let memory = ComputeMemory::new(1 << 26);
        let found = core_number(workers(), &memory, &mut Uninterrupted, graph, relations)?;
        Ok((found.cores.values().to_vec(), found.degeneracy))
    }

    /// Each node's core number, the naive way: for each k, strip nodes of
    /// degree below k until none is left; a node's core number is the
    /// largest k whose core keeps it.
    fn naive(id_space: u64, edges: &Edges, selected: Option<&[usize]>) -> Vec<i64> {
        let mut adjacent = BTreeSet::new();
        for &(relation, s, t, _) in edges {
            if s != t && selected.is_none_or(|selected| selected.contains(&relation)) {
                adjacent.insert((s, t));
                adjacent.insert((t, s));
            }
        }
        let mut numbers = vec![0; id_space as usize];
        for k in 1..id_space as i64 {
            let mut kept: BTreeSet<u64> = (0..id_space).collect();
            loop {
                let degree = |v: u64| kept.iter().filter(|&&u| adjacent.contains(&(v, u))).count();
                let weak: Vec<u64> =
                    kept.iter().copied().filter(|&v| (degree(v) as i64) < k).collect();
                if weak.is_empty() {
                    break;
                }
                weak.iter().for_each(|v| _ = kept.remove(v));
            }
            kept.iter().for_each(|&v| numbers[v as usize] = k);
        }
        numbers
    }

    #[test]
    fn a_clique_with_a_tail_a_self_loop_and_an_isolated_node() {
        // K4 on 0..=3 (some edges both ways), then 3 → 4 → 5, 5 → 5, and 6.
        let mut edges: Edges = vec![(0, 3, 4, 1.0), (0, 4, 5, 1.0), (0, 5, 5, 1.0)];
        for a in 0..4 {
            for b in 0..4 {
                if a != b && (a < b || a == 2) {
                    edges.push((0, a, b, 1.0));
                }
            }
        }
        let graph = reference::graph(7, 1, &edges, false, false);
        assert_eq!(cores(&graph, RelationSelection::All), Ok((vec![3, 3, 3, 3, 1, 1, 0], 3)));
    }

    #[test]
    fn a_graph_without_edges_has_degeneracy_zero() {
        let graph = reference::graph(3, 1, &vec![(0, 1, 1, 1.0)], false, false);
        assert_eq!(cores(&graph, RelationSelection::All), Ok((vec![0, 0, 0], 0)));
        let empty = reference::graph(0, 1, &vec![], false, false);
        assert_eq!(cores(&empty, RelationSelection::All), Ok((vec![], 0)));
    }

    #[test]
    fn an_interrupt_cancels_the_peeling() {
        struct Stop;
        impl Interrupts for Stop {
            fn check(&mut self) -> Result<(), Cancelled> {
                Err(Cancelled)
            }
        }
        let graph = reference::graph(3, 1, &vec![(0, 0, 1, 1.0)], false, false);
        let memory = ComputeMemory::new(1 << 20);
        let result = core_number(workers(), &memory, &mut Stop, &graph, RelationSelection::All);
        assert_eq!(result.map(|_| ()), Err(ComputeError::Cancelled));
    }

    #[test]
    fn a_cancellation_stops_a_long_peeling() {
        let n = 200_000;
        let edges: Edges = (1..n).map(|i| (0, 0, i, 1.0)).collect();
        let graph = reference::graph(n, 1, &edges, false, false);
        let forward = [graph.relations()[0].forward().into()];
        let memory = ComputeMemory::new(1 << 26);
        let cancellation = Cancellation::new();
        let adjacency = Symmetric::new(&memory, &cancellation, &forward, n).unwrap();
        cancellation.cancel();
        let mut degrees = vec![0; n as usize];
        let peeled = peel(&memory, &cancellation, &adjacency, &mut degrees);
        assert_eq!(peeled, Err(ComputeError::Cancelled));
    }

    #[test]
    fn the_adjacency_and_the_peeling_are_charged() {
        let graph = reference::graph(3, 1, &vec![(0, 0, 1, 1.0), (0, 1, 2, 1.0)], false, false);
        let fits = |limit| {
            let memory = ComputeMemory::new(limit);
            let found =
                core_number(workers(), &memory, &mut Uninterrupted, &graph, RelationSelection::All);
            let fitted = found.is_ok();
            drop(found);
            assert_eq!(memory.in_use(), 0, "every charge is released");
            fitted
        };
        // The cores (24 bytes), the adjacency (32 + 4 × 8) and the
        // peeling's buckets (degrees 0..=2: 3 × 8) and order and places
        // (2 × 24).
        let needed = 24 + 64 + 24 + 48;
        assert!(fits(needed));
        assert!(!fits(needed - 1));
    }

    #[test]
    fn held_memory_is_reclaimed_before_the_pool_charges() {
        let edges = vec![(0, 0, 1, 1.0), (0, 1, 2, 1.0), (0, 2, 0, 1.0), (0, 2, 3, 1.0)];
        let graph = reference::graph(4, 1, &edges, false, false);
        let held = 1 << 20;
        let memory = ComputeMemory::reclaiming(held, held, held, Box::new(Exact));
        let found =
            core_number(workers(), &memory, &mut Uninterrupted, &graph, RelationSelection::All);
        assert_eq!(found.map(|found| found.degeneracy), Ok(2));
    }

    #[test]
    fn a_long_path_and_a_hub_peel_in_linear_time() {
        // A path 0 – 1 – … – n and a hub joined to every node of it.
        let n: u64 = 300_000;
        let mut edges: Edges = (0..n).map(|i| (0, i, i + 1, 1.0)).collect();
        edges.extend((0..=n).map(|i| (0, n + 1, i, 1.0)));
        let graph = reference::graph(n + 2, 1, &edges, false, false);
        let (numbers, degeneracy) = cores(&graph, RelationSelection::All).unwrap();
        assert_eq!(degeneracy, 2);
        assert!(numbers.iter().all(|&core| core == 2));
    }

    type Scenario = (u64, usize, Edges, Option<Vec<usize>>, (bool, bool));

    fn scenarios() -> impl Strategy<Value = Scenario> {
        (1u64..10, 1usize..4).prop_flat_map(|(id_space, relations)| {
            let edge = (0..relations, 0..id_space, 0..id_space, Just(1.0));
            (
                Just(id_space),
                Just(relations),
                prop::collection::vec(edge, 0..50),
                prop::option::of(prop::collection::vec(0..relations, 0..2)),
                any::<(bool, bool)>(),
            )
        })
    }

    proptest! {
        #[test]
        fn core_numbers_match_naive_stripping((id_space, relations, edges, selected, (wide, store_merged)) in scenarios()) {
            let indexes = reference::relations(relations);
            let mut list = EdgeList::new(false);
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
            let expected = naive(id_space, &edges, selected.as_deref());
            let degeneracy = expected.iter().copied().max().unwrap_or(0);
            prop_assert_eq!(cores(&graph, selection), Ok((expected, degeneracy)));
        }
    }
}
