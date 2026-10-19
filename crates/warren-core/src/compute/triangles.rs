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

use super::{ComputeError, ComputeMemory, MemoryCharge, Vector, Workers};
use crate::graph::{AnyCsr, DenseId, GraphCsr, RelationSelection};
use crate::interrupts::{Cancellation, Cancelled, Interrupts};

/// The nodes one task handles; a task checks the cancellation flag first.
/// Small under test, so tests cross task boundaries.
const NODES_PER_TASK: usize = if cfg!(test) { 3 } else { 4096 };

/// How often the sequential build checks the cancellation flag, in nodes.
const NODES_PER_CHECK: u64 = 1 << 16;

/// Each node's triangle count over the selected relations, as an `i64`
/// vector.
pub fn triangles<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &GraphCsr,
    relations: RelationSelection<'_>,
) -> Result<Vector<i64>, ComputeError> {
    let selected = graph
        .selected(relations)
        .map_err(|relation| ComputeError::UnknownRelation { relation: relation.index() })?;
    let forward: Vec<&AnyCsr> = selected.iter().map(|relation| relation.forward()).collect();
    let id_space = graph.all_relations().forward().id_space();
    let mut counts = Vector::filled(memory, id_space, 1, 0i64)?;
    let values = counts.values_mut();
    workers.run(interrupts, |cancellation| {
        let adjacency = Symmetric::new(memory, cancellation, &forward, id_space)?;
        adjacency.count(cancellation, values)
    })?;
    Ok(counts)
}

/// Each node's neighbours in either direction (self-loops dropped), each
/// row sorted; a neighbour in several relations or both directions
/// repeats.
struct Symmetric {
    /// `id_space + 1` positions into `targets`.
    offsets: Vec<u64>,
    targets: Vec<DenseId>,
    _charges: [MemoryCharge; 2],
}

impl Symmetric {
    fn new(
        memory: &ComputeMemory,
        cancellation: &Cancellation,
        forward: &[&AnyCsr],
        id_space: u64,
    ) -> Result<Self, ComputeError> {
        let each_edge = |f: &mut dyn FnMut(DenseId, DenseId)| -> Result<(), Cancelled> {
            for u in 0..id_space {
                if u % NODES_PER_CHECK == 0 && cancellation.is_cancelled() {
                    return Err(Cancelled);
                }
                for csr in forward {
                    csr.neighbours(u).filter(|&v| v != u).for_each(|v| f(u, v));
                }
            }
            Ok(())
        };
        let (mut offsets, offsets_charge) = memory.filled(id_space as usize + 1, 0u64)?;
        each_edge(&mut |u, v| {
            offsets[u as usize] += 1;
            offsets[v as usize] += 1;
        })?;
        // Running ends; placing each entry then moves a row's end to its
        // start.
        let mut end = 0;
        for offset in &mut offsets[..id_space as usize] {
            end += *offset;
            *offset = end;
        }
        offsets[id_space as usize] = end;
        let (mut targets, targets_charge) = memory.filled(end as usize, 0 as DenseId)?;
        each_edge(&mut |u, v| {
            for (row, target) in [(u, v), (v, u)] {
                let offset = &mut offsets[row as usize];
                *offset -= 1;
                targets[*offset as usize] = target;
            }
        })?;
        let mut symmetric = Self { offsets, targets, _charges: [offsets_charge, targets_charge] };
        symmetric.sort(cancellation)?;
        Ok(symmetric)
    }

    /// The row of `node`.
    fn row(&self, node: usize) -> &[DenseId] {
        &self.targets[self.offsets[node] as usize..self.offsets[node + 1] as usize]
    }

    /// Sorts every row, in parallel tasks of whole rows.
    fn sort(&mut self, cancellation: &Cancellation) -> Result<(), Cancelled> {
        let offsets = &self.offsets;
        let nodes = offsets.len() - 1;
        let mut tasks = Vec::new();
        let mut rest = self.targets.as_mut_slice();
        for first in (0..nodes).step_by(NODES_PER_TASK) {
            let last = nodes.min(first + NODES_PER_TASK);
            let len = (offsets[last] - offsets[first]) as usize;
            let (chunk, tail) = std::mem::take(&mut rest).split_at_mut(len);
            tasks.push((first..last, chunk));
            rest = tail;
        }
        tasks.into_par_iter().try_for_each(|(nodes, chunk)| {
            if cancellation.is_cancelled() {
                return Err(Cancelled);
            }
            let base = offsets[nodes.start];
            for node in nodes {
                let row = (offsets[node] - base) as usize..(offsets[node + 1] - base) as usize;
                chunk[row].sort_unstable();
            }
            Ok(())
        })
    }

    /// Writes each node's triangle count to `counts`.
    fn count(&self, cancellation: &Cancellation, counts: &mut [i64]) -> Result<(), ComputeError> {
        counts.par_chunks_mut(NODES_PER_TASK).enumerate().try_for_each(|(task, counts)| {
            let mut work = Work::new(cancellation);
            work.check()?;
            for (offset, count) in counts.iter_mut().enumerate() {
                *count = self.triangles_of(task * NODES_PER_TASK + offset, &mut work)?;
            }
            Ok(())
        })
    }

    /// The triangles `node` is in: half the neighbours its neighbours share
    /// with it.
    fn triangles_of(&self, node: usize, work: &mut Work<'_>) -> Result<i64, Cancelled> {
        let row = self.row(node);
        let mut pairs = 0;
        for v in distinct(row) {
            pairs += common(row, self.row(v as usize), work)?;
        }
        Ok(i64::try_from(pairs / 2).unwrap_or(i64::MAX))
    }
}

/// The work a task has done since it last checked its cancellation flag,
/// in row entries visited: it checks every [`WORK_PER_CHECK`], so a hub's
/// row cannot outlast a cancel (DESIGN.md §6.9).
struct Work<'c> {
    cancellation: &'c Cancellation,
    since_check: u64,
}

/// Row entries visited between cancellation checks.
const WORK_PER_CHECK: u64 = 1 << 16;

impl<'c> Work<'c> {
    fn new(cancellation: &'c Cancellation) -> Self {
        Self { cancellation, since_check: 0 }
    }

    /// Checks the flag now.
    fn check(&mut self) -> Result<(), Cancelled> {
        self.since_check = 0;
        match self.cancellation.is_cancelled() {
            true => Err(Cancelled),
            false => Ok(()),
        }
    }

    /// Records `units` of work, checking the flag once enough is done.
    fn spend(&mut self, units: u64) -> Result<(), Cancelled> {
        self.since_check += units;
        match self.since_check >= WORK_PER_CHECK {
            true => self.check(),
            false => Ok(()),
        }
    }
}

/// The distinct values of a sorted row.
fn distinct(row: &[DenseId]) -> impl Iterator<Item = DenseId> + '_ {
    row.iter().enumerate().filter(|&(i, v)| i == 0 || row[i - 1] != *v).map(|(_, &v)| v)
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
    use crate::compute::reference::{self, Edges};
    use crate::compute::{ComputeError, ComputeMemory, Workers};
    use crate::graph::{EdgeList, GraphCsr, RelationSelection};
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

    fn scenarios() -> impl Strategy<Value = (u64, usize, Edges, Option<Vec<usize>>, bool)> {
        (1u64..12, 1usize..3).prop_flat_map(|(id_space, relations)| {
            let edge = (0..relations, 0..id_space, 0..id_space, Just(1.0));
            (
                Just(id_space),
                Just(relations),
                prop::collection::vec(edge, 0..60),
                prop::option::of(prop::collection::vec(0..relations, 0..2)),
                any::<bool>(),
            )
        })
    }

    proptest! {
        #[test]
        fn counts_match_the_naive_triples((id_space, relations, edges, selected, wide) in scenarios()) {
            let indexes = reference::relations(relations);
            let mut list = EdgeList::new(false);
            for &(r, s, t, w) in &edges {
                list.push(indexes[r], s, t, w).unwrap();
            }
            let graph = match wide {
                true => GraphCsr::build_with::<u64>(list, relations, id_space, false),
                false => GraphCsr::build(list, relations, id_space, false),
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
        let forward = [graph.all_relations().forward()];
        let memory = ComputeMemory::new(1 << 26);
        let cancellation = Cancellation::new();
        let symmetric = Symmetric::new(&memory, &cancellation, &forward, 100_001).unwrap();
        cancellation.cancel();
        // The hub's row alone is more work than a check is apart.
        assert_eq!(symmetric.triangles_of(0, &mut Work::new(&cancellation)), Err(Cancelled));
        // A leaf's row is less, and finishes.
        assert_eq!(symmetric.triangles_of(2, &mut Work::new(&cancellation)), Ok(2));
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
