//! Per-node triangle counts (DESIGN.md §6.7, `warren.triangles`): how many
//! triangles of the undirected simple graph underlying the selected
//! relations each node is in, as NetworkX's `triangles` counts them.
//!
//! The kernel orients the graph by degree once ([`oriented`]: nodes
//! ranked by (degree, dense id) and named by rank, each edge kept once,
//! from its lower-ranked endpoint to the other, charged to the budget; no
//! reverse CSR is needed), then finds each triangle once, from its
//! lowest-ranked node, in parallel tasks of nodes. A search from `u` marks
//! `u`'s row in its thread's [`marks`], then reads the row of each `v` in
//! it: each marked `w` there closes a triangle `u`, `v`, `w`. `u` and
//! `v`'s counts take what a search found at once, and so does each `w`'s
//! at its end, so the shared counts are touched at most three times per
//! oriented edge, not once per triangle. A search costs its row's length
//! plus its row's rows' lengths, and no row is longer than about `√(2M)`:
//! about `M · √M` at worst, near-linear on sparse graphs, hubs or not. A
//! task checks its cancellation flag every 65,536 row entries it reads.

mod marks;
mod oriented;

use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

use self::marks::Marks;
use self::oriented::Oriented;
use super::symmetric::selected_rows;
use super::{ComputeError, ComputeMemory, Vector, Workers};
use crate::graph::{GraphCsr, RelationSelection};
use crate::interrupts::{Cancellation, Cancelled, Interrupts, Work};

/// The nodes (consecutive ranks) one task handles; a task checks the
/// cancellation flag first. Few, since a search's cost grows with rank up
/// to the hubs' and the tasks must balance (4,096 left four threads
/// waiting on a few tasks). Small under test, so tests cross task
/// boundaries.
const NODES_PER_TASK: usize = if cfg!(test) { 3 } else { 256 };

/// Each node's triangle count over the selected relations, as an `i64`
/// vector.
pub fn triangles<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &GraphCsr,
    relations: RelationSelection<'_>,
) -> Result<Vector<i64>, ComputeError> {
    let forward = selected_rows(graph, relations)?;
    let id_space = graph.all_relations().forward().id_space();
    let mut counts = Vector::filled(memory, id_space, 1, 0i64)?;
    let values = counts.values_mut()?;
    let threads = workers.threads();
    // The adjacency, the triangles found (a count per node) and the marks
    // are charged on the pool, which reclaims no held memory (`memory`), so room for them is
    // reclaimed here, on the caller's thread.
    let adjacency = Oriented::bytes(&forward, id_space);
    let found = id_space.saturating_mul(size_of::<u64>() as u64);
    let marks = Marks::bytes(id_space, threads);
    memory.reclaim_for(adjacency.saturating_add(found).saturating_add(marks));
    workers.run(interrupts, |cancellation| {
        let adjacency = Oriented::new(memory, cancellation, &forward, id_space)?;
        count(memory, &adjacency, cancellation, threads, values)
    })?;
    Ok(counts)
}

/// Writes each node's triangle count to `counts` (by dense id).
fn count(
    memory: &ComputeMemory,
    adjacency: &Oriented,
    cancellation: &Cancellation,
    threads: usize,
    counts: &mut [i64],
) -> Result<(), ComputeError> {
    let nodes = adjacency.nodes();
    let (found, _found_charge) = memory.filled_with(nodes, || AtomicU64::new(0))?;
    let marks = Marks::new(memory, nodes, threads)?;
    (0..nodes.div_ceil(NODES_PER_TASK)).into_par_iter().try_for_each(|task| {
        let mut work = Work::new(cancellation);
        work.check()?;
        let first = task * NODES_PER_TASK;
        marks.with(|marks| {
            (first..nodes.min(first + NODES_PER_TASK))
                .try_for_each(|node| search(adjacency, &found, marks, node, &mut work))
        })
    })?;
    counts.par_iter_mut().zip(adjacency.ranks()).for_each(|(count, &rank)| {
        *count = i64::try_from(found[rank as usize].load(Ordering::Relaxed)).unwrap_or(i64::MAX);
    });
    Ok(())
}

/// Finds the triangles whose lowest-ranked node is `u` (a rank, as all
/// nodes here are), each once, and adds them to `found`: for each `v` in
/// `u`'s row, the nodes `w` in both `u`'s row (marked in `marks`, which
/// it leaves all zero unless it fails) and `v`'s.
fn search(
    adjacency: &Oriented,
    found: &[AtomicU64],
    marks: &mut [u32],
    u: usize,
    work: &mut Work<'_>,
) -> Result<(), Cancelled> {
    let row = adjacency.row(u);
    for &v in row {
        marks[v as usize] = 1;
    }
    let mut at_u = 0;
    for &v in row {
        let next = adjacency.row(v as usize);
        let mut at_v = 0;
        for &w in next {
            let mark = &mut marks[w as usize];
            if *mark != 0 {
                *mark += 1;
                at_v += 1;
            }
        }
        work.spend(next.len() as u64 + 1)?;
        if at_v > 0 {
            found[v as usize].fetch_add(at_v, Ordering::Relaxed);
            at_u += at_v;
        }
    }
    for &w in row {
        let through = std::mem::take(&mut marks[w as usize]) - 1;
        if through > 0 {
            found[w as usize].fetch_add(u64::from(through), Ordering::Relaxed);
        }
    }
    if at_u > 0 {
        found[u].fetch_add(at_u, Ordering::Relaxed);
    }
    Ok(())
}

// Not under Miri: the compute pool's work stealing runs through
// crossbeam-epoch, which Miri's Stacked Borrows model rejects (a known
// report against crossbeam-epoch 0.9, not this crate, which has no unsafe).
#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::OnceLock;

    use proptest::prelude::*;

    use super::*;
    use crate::compute::memory::Exact;
    use crate::compute::reference::{self, Edges};
    use crate::compute::{ComputeError, ComputeMemory, Workers};
    use crate::graph::{CsrLayout, EdgeList, GraphCsr, RelationSelection};
    use crate::interrupts::{Cancelled, Interrupts, Uninterrupted};

    fn workers() -> &'static Workers {
        static WORKERS: OnceLock<Workers> = OnceLock::new();
        WORKERS.get_or_init(|| Workers::new(3).unwrap())
    }

    fn count(graph: &GraphCsr, relations: RelationSelection<'_>) -> Result<Vec<i64>, ComputeError> {
        let memory = ComputeMemory::new(1 << 24);
        let counted = triangles(workers(), &memory, &mut Uninterrupted, graph, relations)?;
        Ok(counted.values().to_vec())
    }

    /// Each node's triangles, the naive way: every triple of distinct nodes
    /// pairwise adjacent in either direction.
    fn naive(id_space: u64, edges: &Edges, selected: Option<&[usize]>) -> Vec<i64> {
        let mut adjacent = BTreeSet::new();
        for &(relation, s, t, _) in edges {
            if s != t && selected.is_none_or(|selected| selected.contains(&relation)) {
                adjacent.insert((s, t));
                adjacent.insert((t, s));
            }
        }
        let mut counts = vec![0; id_space as usize];
        for a in 0..id_space {
            for b in a + 1..id_space {
                for c in b + 1..id_space {
                    let edge = |x, y| adjacent.contains(&(x, y));
                    if edge(a, b) && edge(b, c) && edge(a, c) {
                        for node in [a, b, c] {
                            counts[node as usize] += 1;
                        }
                    }
                }
            }
        }
        counts
    }

    #[test]
    fn a_triangle_with_a_tail_and_a_self_loop() {
        // 0 → 1 → 2 → 0 in both directions once, 2 → 3, 3 → 3.
        let edges = vec![
            (0, 0, 1, 1.0),
            (0, 1, 2, 1.0),
            (0, 0, 2, 1.0),
            (0, 2, 0, 1.0),
            (0, 2, 3, 1.0),
            (0, 3, 3, 1.0),
        ];
        let graph = reference::graph(5, 1, &edges, false, false);
        assert_eq!(count(&graph, RelationSelection::All), Ok(vec![1, 1, 1, 0, 0]));
    }

    #[test]
    fn an_interrupt_cancels_the_count() {
        struct Stop;
        impl Interrupts for Stop {
            fn check(&mut self) -> Result<(), Cancelled> {
                Err(Cancelled)
            }
        }
        let graph = reference::graph(3, 1, &vec![(0, 0, 1, 1.0)], false, false);
        let memory = ComputeMemory::new(1 << 20);
        let result = triangles(workers(), &memory, &mut Stop, &graph, RelationSelection::All);
        assert_eq!(result.map(|_| ()), Err(ComputeError::Cancelled));
    }

    #[test]
    fn the_oriented_adjacency_is_charged() {
        let graph = reference::graph(3, 1, &vec![(0, 0, 1, 1.0), (0, 1, 2, 1.0)], false, false);
        // Room for the counts (24 bytes) and offsets (32), not the 4 entries.
        let memory = ComputeMemory::new(60);
        let result =
            triangles(workers(), &memory, &mut Uninterrupted, &graph, RelationSelection::All);
        assert!(matches!(result, Err(ComputeError::MemoryLimit { .. })));
        assert_eq!(memory.in_use(), 0);
    }

    #[test]
    fn held_memory_is_reclaimed_before_the_oriented_adjacency_is_built() {
        // 0 → 1 → 2 → 0, 2 → 3: the adjacency is charged on the pool.
        let edges = vec![(0, 0, 1, 1.0), (0, 1, 2, 1.0), (0, 2, 0, 1.0), (0, 2, 3, 1.0)];
        let graph = reference::graph(4, 1, &edges, false, false);
        let held = 1 << 20;
        let memory = ComputeMemory::reclaiming(held, held, held, Box::new(Exact));
        let counted =
            triangles(workers(), &memory, &mut Uninterrupted, &graph, RelationSelection::All);
        assert_eq!(counted.map(|counts| counts.values().to_vec()), Ok(vec![1, 1, 1, 0]));
    }

    type Scenario = (u64, usize, Edges, Option<Vec<usize>>, (bool, bool));

    fn scenarios() -> impl Strategy<Value = Scenario> {
        (1u64..12, 1usize..4).prop_flat_map(|(id_space, relations)| {
            let edge = (0..relations, 0..id_space, 0..id_space, Just(1.0));
            (
                Just(id_space),
                Just(relations),
                prop::collection::vec(edge, 0..60),
                prop::option::of(prop::collection::vec(0..relations, 0..2)),
                any::<(bool, bool)>(),
            )
        })
    }

    /// The kernel's counts over a scenario's graph, and the naive ones.
    fn counted_and_naive(scenario: Scenario) -> (Result<Vec<i64>, ComputeError>, Vec<i64>) {
        let (id_space, relations, edges, selected, (wide, store_merged)) = scenario;
        let indexes = reference::relations(relations);
        let mut list = EdgeList::new(false);
        for &(r, s, t, w) in &edges {
            list.push(indexes[r], s, t, w).unwrap();
        }
        let layout = CsrLayout { build_reverse: false, store_merged };
        let graph = match wide {
            true => GraphCsr::build_with::<u64>(list, relations, id_space, layout),
            false => GraphCsr::build_as(list, relations, id_space, layout),
        }
        .unwrap();
        let chosen: Option<Vec<_>> =
            selected.as_ref().map(|s| s.iter().map(|&r| indexes[r]).collect());
        let selection = chosen.as_deref().map_or(RelationSelection::All, RelationSelection::Only);
        (count(&graph, selection), naive(id_space, &edges, selected.as_deref()))
    }

    /// Few edges per relation among many dense ids, so the relations'
    /// offsets are sparse and most ids have no edges, as tombstoned ones.
    fn sparse_scenarios() -> impl Strategy<Value = Scenario> {
        (40u64..64, 4usize..8).prop_flat_map(|(id_space, relations)| {
            let edge = (0..relations, 0..12u64, 0..12u64, Just(1.0));
            let spread = move |(r, s, t, w): (usize, u64, u64, f64)| {
                (r, s * 5 % id_space, t * 5 % id_space, w)
            };
            (
                Just(id_space),
                Just(relations),
                prop::collection::vec(edge.prop_map(spread), 0..40),
                prop::option::of(prop::collection::vec(0..relations, 0..4)),
                any::<(bool, bool)>(),
            )
        })
    }

    proptest! {
        #[test]
        fn counts_match_the_naive_triples_over_sparse_relations(scenario in sparse_scenarios()) {
            let (counted, naive) = counted_and_naive(scenario);
            prop_assert_eq!(counted, Ok(naive));
        }

        #[test]
        fn counts_match_the_naive_triples(scenario in scenarios()) {
            let (counted, naive) = counted_and_naive(scenario);
            prop_assert_eq!(counted, Ok(naive));
        }
    }

    /// A star: hub 0 joined to leaves 1..=n, which a path also joins, so
    /// the hub is in n - 1 triangles and each leaf in one or two.
    fn star(n: u64) -> GraphCsr {
        let mut edges: Edges = (1..=n).map(|i| (0, 0, i, 1.0)).collect();
        edges.extend((1..n).map(|i| (0, i, i + 1, 1.0)));
        reference::graph(n + 1, 1, &edges, false, false)
    }

    /// The smallest `warren.max_compute_memory` under which `triangles`
    /// over all relations succeeds.
    fn least_budget(graph: &GraphCsr) -> u64 {
        let fits = |limit| {
            let memory = ComputeMemory::new(limit);
            triangles(workers(), &memory, &mut Uninterrupted, graph, RelationSelection::All).is_ok()
        };
        let (mut low, mut high) = (0, 1 << 24);
        while low < high {
            let mid = (low + high) / 2;
            if fits(mid) {
                high = mid
            } else {
                low = mid + 1
            }
        }
        low
    }

    #[test]
    fn the_charge_over_all_relations_does_not_depend_on_the_merged_csr() {
        // Five relations holding the same edges: each edge is in all five.
        let ring: Edges = (0..200).map(|i| (0, i, (i + 1) % 200, 1.0)).collect();
        let chords: Edges = (0..200).map(|i| (0, i, (i + 2) % 200, 1.0)).collect();
        let edges: Edges = (0..5)
            .flat_map(|r| ring.iter().chain(&chords).map(move |&(_, s, t, w)| (r, s, t, w)))
            .collect();
        let build = |store_merged| {
            let layout = CsrLayout { build_reverse: false, store_merged };
            reference::graph_as(200, 5, &edges, false, layout)
        };
        let (stored, unmerged) = (build(true), build(false));
        assert_eq!(
            count(&unmerged, RelationSelection::All),
            count(&stored, RelationSelection::All)
        );
        assert_eq!(least_budget(&unmerged), least_budget(&stored));
    }

    /// A clique of `n` nodes, each pair joined in both directions.
    fn clique(n: u64) -> GraphCsr {
        let edges: Edges = (0..n)
            .flat_map(|a| (0..n).filter(move |&b| b != a).map(move |b| (0, a, b, 1.0)))
            .collect();
        reference::graph(n, 1, &edges, false, false)
    }

    #[test]
    fn the_oriented_adjacency_holds_each_stored_edge_once() {
        // 10 nodes, 90 stored edges: the counts (80 bytes), the ranks (80),
        // the offsets (88), one entry per stored edge (720), the triangles
        // found (80) and the marks of the pool's 3 threads (3 × 40).
        let graph = clique(10);
        assert_eq!(count(&graph, RelationSelection::All).unwrap(), vec![36; 10]);
        assert_eq!(least_budget(&graph), 80 + 80 + 88 + 720 + 80 + 3 * 40);
    }

    #[test]
    fn a_hub_costs_its_degree_not_its_square() {
        // Merging the hub's row with every leaf's would take n² steps.
        let n = 200_000;
        let counts = count(&star(n), RelationSelection::All).unwrap();
        assert_eq!(counts[0], n as i64 - 1);
        assert_eq!((counts[1], counts[2], counts[n as usize]), (1, 2, 1));
    }

    #[test]
    fn a_cancellation_stops_a_long_row_partway() {
        // All degrees equal, so ranks follow dense ids: node 0's row holds
        // the other 399 nodes, node 398's only node 399.
        let graph = clique(400);
        let forward = [graph.all_relations().forward()];
        let memory = ComputeMemory::new(1 << 26);
        let cancellation = Cancellation::new();
        let oriented = Oriented::new(&memory, &cancellation, &forward, 400).unwrap();
        let found: Vec<_> = (0..400).map(|_| AtomicU64::new(0)).collect();
        let mut marks = vec![0; 400];
        cancellation.cancel();
        // Node 0's rows are more work than a check is apart.
        let mut work = Work::new(&cancellation);
        assert_eq!(search(&oriented, &found, &mut marks, 0, &mut work), Err(Cancelled));
        // Node 398's are less, and finish.
        let (mut marks, mut work) = (vec![0; 400], Work::new(&cancellation));
        assert_eq!(search(&oriented, &found, &mut marks, 398, &mut work), Ok(()));
        assert_eq!(marks, vec![0; 400]);
    }

    #[test]
    fn rows_span_many_tasks() {
        // A wheel: hub 0 joined to a cycle 1..n; each rim node is in two
        // triangles, the hub in n.
        let n: u64 = 5000;
        let mut edges: Edges = (1..=n).map(|i| (0, 0, i, 1.0)).collect();
        edges.extend((1..=n).map(|i| (0, i, i % n + 1, 1.0)));
        let graph = reference::graph(n + 1, 1, &edges, false, true);
        let counts = count(&graph, RelationSelection::All).unwrap();
        assert_eq!(counts[0], n as i64);
        assert!(counts[1..].iter().all(|&c| c == 2));
    }
}
