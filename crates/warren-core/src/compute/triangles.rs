//! Per-node triangle counts (DESIGN.md §6.7, `warren.triangles`): how many
//! triangles of the undirected simple graph underlying the selected
//! relations each node is in, as NetworkX's `triangles` counts them.
//!
//! The kernel builds the symmetric adjacency once (each node's neighbours
//! in either direction, self-loops dropped, in a CSR charged to the
//! budget; no reverse CSR is needed), then counts in parallel: a node `u`
//! is in `Σ_{v ∈ N(u)} |N(u) ∩ N(v)| / 2` triangles, each intersection a
//! merge of two sorted rows, or, when one row is much longer (a hub's),
//! a binary search of the shorter's values in it. The cost is about
//! `Σ_u Σ_{v ∈ N(u)} min(d_u, d_v) · log max(d_u, d_v)`, and a task checks
//! its cancellation flag every 65,536 row entries it visits.

use std::cmp::Ordering;

use rayon::prelude::*;

use super::symmetric::{distinct, selected_rows, Symmetric};
use super::{ComputeError, ComputeMemory, Vector, Workers};
use crate::graph::{DenseId, GraphCsr, RelationSelection};
use crate::interrupts::{Cancellation, Cancelled, Interrupts, Work};

/// The nodes one task handles; a task checks the cancellation flag first.
/// Small under test, so tests cross task boundaries.
const NODES_PER_TASK: usize = if cfg!(test) { 3 } else { 4096 };

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
    let values = counts.values_mut();
    // The adjacency is charged on the pool, which reclaims no held memory
    // (`memory`), so room for it is reclaimed here, on the caller's thread.
    memory.reclaim_for(Symmetric::bytes(&forward, id_space));
    workers.run(interrupts, |cancellation| {
        let adjacency = Symmetric::new(memory, cancellation, &forward, id_space)?;
        count(&adjacency, cancellation, values)
    })?;
    Ok(counts)
}

/// Writes each node's triangle count to `counts`.
fn count(
    adjacency: &Symmetric,
    cancellation: &Cancellation,
    counts: &mut [i64],
) -> Result<(), ComputeError> {
    counts.par_chunks_mut(NODES_PER_TASK).enumerate().try_for_each(|(task, counts)| {
        let mut work = Work::new(cancellation);
        work.check()?;
        for (offset, count) in counts.iter_mut().enumerate() {
            *count = triangles_of(adjacency, task * NODES_PER_TASK + offset, &mut work)?;
        }
        Ok(())
    })
}

/// The triangles `node` is in: half the neighbours its neighbours share
/// with it.
fn triangles_of(adjacency: &Symmetric, node: usize, work: &mut Work<'_>) -> Result<i64, Cancelled> {
    let row = adjacency.row(node);
    let mut pairs = 0;
    for v in distinct(row) {
        pairs += common(row, adjacency.row(v as usize), work)?;
    }
    Ok(i64::try_from(pairs / 2).unwrap_or(i64::MAX))
}

/// How much longer one row must be than the other before the shorter is
/// searched into the longer instead of merged with it.
const SKEW: usize = 16;

/// The number of distinct values two sorted rows share: a merge when they
/// are of similar length, else a binary search of each value of the
/// shorter in the rest of the longer, so a hub's row costs about its
/// neighbour's length, not its own. Its work is spent on `work`.
fn common(a: &[DenseId], b: &[DenseId], work: &mut Work<'_>) -> Result<u64, Cancelled> {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if short.len().saturating_mul(SKEW) < long.len() {
        let mut rest = long;
        let mut shared = 0;
        for value in distinct(short) {
            let at = rest.partition_point(|&x| x < value);
            shared += u64::from(rest.get(at) == Some(&value));
            rest = &rest[at..];
            work.spend(1 + (usize::BITS - long.len().leading_zeros()) as u64)?;
        }
        return Ok(shared);
    }
    let (mut i, mut j, mut shared) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                let value = a[i];
                shared += 1;
                while a.get(i) == Some(&value) {
                    i += 1;
                }
                while b.get(j) == Some(&value) {
                    j += 1;
                }
            }
        }
    }
    work.spend((a.len() + b.len()) as u64)?;
    Ok(shared)
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
    fn the_symmetric_adjacency_is_charged() {
        let graph = reference::graph(3, 1, &vec![(0, 0, 1, 1.0), (0, 1, 2, 1.0)], false, false);
        // Room for the counts (24 bytes) and offsets (32), not the 4 entries.
        let memory = ComputeMemory::new(60);
        let result =
            triangles(workers(), &memory, &mut Uninterrupted, &graph, RelationSelection::All);
        assert!(matches!(result, Err(ComputeError::MemoryLimit { .. })));
        assert_eq!(memory.in_use(), 0);
    }

    #[test]
    fn held_memory_is_reclaimed_before_the_symmetric_adjacency_is_built() {
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

    proptest! {
        #[test]
        fn counts_match_the_naive_triples((id_space, relations, edges, selected, (wide, store_merged)) in scenarios()) {
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
            prop_assert_eq!(count(&graph, selection), Ok(naive(id_space, &edges, selected.as_deref())));
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

    #[test]
    fn a_hub_costs_its_degree_not_its_square() {
        // Merging the hub's row with every leaf's would take n² steps.
        let n = 200_000;
        let counts = count(&star(n), RelationSelection::All).unwrap();
        assert_eq!(counts[0], n as i64 - 1);
        assert_eq!((counts[1], counts[2], counts[n as usize]), (1, 2, 1));
    }

    #[test]
    fn a_cancellation_stops_a_hub_row_partway() {
        let graph = star(100_000);
        let forward = [graph.relations()[0].forward().into()];
        let memory = ComputeMemory::new(1 << 26);
        let cancellation = Cancellation::new();
        let symmetric = Symmetric::new(&memory, &cancellation, &forward, 100_001).unwrap();
        cancellation.cancel();
        // The hub's row alone is more work than a check is apart.
        assert_eq!(triangles_of(&symmetric, 0, &mut Work::new(&cancellation)), Err(Cancelled));
        // A leaf's row is less, and finishes.
        assert_eq!(triangles_of(&symmetric, 2, &mut Work::new(&cancellation)), Ok(2));
    }

    proptest! {
        #[test]
        fn intersections_of_skewed_rows_count_distinct_shared_values(
            mut a in prop::collection::vec(0u64..400, 0..300),
            mut b in prop::collection::vec(0u64..400, 0..12),
        ) {
            a.sort_unstable();
            b.sort_unstable();
            let shared = a.iter().collect::<BTreeSet<_>>().intersection(&b.iter().collect()).count() as u64;
            let cancellation = Cancellation::new();
            let mut work = Work::new(&cancellation);
            prop_assert_eq!(common(&a, &b, &mut work), Ok(shared));
            prop_assert_eq!(common(&b, &a, &mut work), Ok(shared));
        }
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
