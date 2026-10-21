//! Strongly connected components (DESIGN.md §6.7,
//! `warren.strong_components_vector`): each node labelled by the smallest
//! dense id of its strongly connected component over the selected
//! relations' edges, as `components` labels weak components.
//!
//! Tarjan's algorithm, made iterative so that a path of millions of nodes
//! cannot overflow the stack: an explicit call stack holds each open node
//! and its position in its rows. It reads the forward CSRs only (no
//! reverse CSR is needed), on one thread of the pool, checking its
//! cancellation flag every 65,536 nodes and edges. Besides the output it
//! keeps one lowlink per node and the two stacks, charged as they grow.

use std::mem::size_of;

use super::memory::MemoryCharge;
use super::{ComputeError, ComputeMemory, Vector, Workers};
use crate::graph::{AnyCsr, DenseId, GraphCsr, RelationSelection};
use crate::interrupts::{Cancellation, Interrupts, Work};

/// A lowlink that marks a node whose component is labelled.
const DONE: u64 = u64::MAX;

/// Each node's strong component label: the smallest dense id in its
/// strongly connected component over the selected relations (a node on
/// no cycle, tombstoned nodes included, is its own component).
pub fn strong_components<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &GraphCsr,
    relations: RelationSelection<'_>,
) -> Result<Vector<i64>, ComputeError> {
    let csrs: Vec<&AnyCsr> = graph
        .selected(relations)
        .map_err(|relation| ComputeError::UnknownRelation { relation: relation.index() })?
        .into_iter()
        .map(|relation| relation.forward())
        .collect();
    let id_space = graph.all_relations().forward().id_space();
    let mut labels = Vector::filled(memory, id_space, 1, 0i64)?;
    let values = labels.values_mut();
    // The lowlinks and the stacks are charged on the pool, which reclaims
    // no held memory, so room for them at their largest is reclaimed here.
    let per_node = size_of::<u64>() + size_of::<DenseId>() + size_of::<Call>();
    memory.reclaim_for(id_space.saturating_mul(per_node as u64));
    workers.run(interrupts, |cancellation| label(memory, cancellation, &csrs, values))?;
    Ok(labels)
}

/// An open node of the depth-first search and the position of the next
/// edge to follow across its rows in every CSR.
type Call = (DenseId, u64);

/// A stack charged to the budget as it grows.
struct Stack<T> {
    items: Vec<T>,
    charge: MemoryCharge,
}

impl<T> Stack<T> {
    fn new(memory: &ComputeMemory) -> Result<Self, ComputeError> {
        Ok(Self { items: Vec::new(), charge: memory.charge(0)? })
    }

    fn push(&mut self, item: T) -> Result<(), ComputeError> {
        self.charge.reserve_one(&mut self.items)?;
        self.items.push(item);
        Ok(())
    }
}

/// The search's state: `labels` holds each open node's visit index,
/// counted from one, until its component is labelled; `low` is 0 for a
/// node not yet visited, [`DONE`] once labelled, else its lowlink.
struct Search<'a> {
    labels: &'a mut [i64],
    low: Vec<u64>,
    visited: u64,
    open: Stack<DenseId>,
    calls: Stack<Call>,
}

impl Search<'_> {
    fn visit(&mut self, node: DenseId) -> Result<(), ComputeError> {
        self.visited += 1;
        self.labels[node as usize] = self.visited as i64;
        self.low[node as usize] = self.visited;
        self.open.push(node)?;
        self.calls.push((node, 0))
    }

    /// Closes `node`: if it is its component's root, labels the component
    /// (the open nodes from it up) with its smallest dense id.
    fn close(&mut self, node: DenseId) {
        if self.low[node as usize] != self.labels[node as usize] as u64 {
            return;
        }
        let open = &mut self.open.items;
        let from = open.iter().rposition(|&v| v == node).unwrap_or(0);
        let smallest = open[from..].iter().copied().min().unwrap_or(node);
        for &member in &open[from..] {
            self.labels[member as usize] = smallest as i64;
            self.low[member as usize] = DONE;
        }
        open.truncate(from);
    }
}

/// Writes each node's component label to `labels`.
fn label(
    memory: &ComputeMemory,
    cancellation: &Cancellation,
    csrs: &[&AnyCsr],
    labels: &mut [i64],
) -> Result<(), ComputeError> {
    let (low, _low_charge) = memory.filled(labels.len(), 0u64)?;
    let mut search =
        Search { labels, low, visited: 0, open: Stack::new(memory)?, calls: Stack::new(memory)? };
    let mut work = Work::new(cancellation);
    for root in 0..search.labels.len() as DenseId {
        work.spend(1)?;
        if search.low[root as usize] != 0 {
            continue;
        }
        search.visit(root)?;
        while let Some(&(node, next)) = search.calls.items.last() {
            work.spend(1)?;
            match target(csrs, node, next) {
                Some(target) => {
                    if let Some(call) = search.calls.items.last_mut() {
                        call.1 += 1;
                    }
                    match search.low[target as usize] {
                        0 => search.visit(target)?,
                        DONE => {}
                        _ => {
                            let index = search.labels[target as usize] as u64;
                            let low = &mut search.low[node as usize];
                            *low = (*low).min(index);
                        }
                    }
                }
                None => {
                    search.calls.items.pop();
                    search.close(node);
                    if let Some(&(parent, _)) = search.calls.items.last() {
                        let reached = search.low[node as usize];
                        let low = &mut search.low[parent as usize];
                        *low = (*low).min(reached);
                    }
                }
            }
        }
    }
    Ok(())
}

/// The target at `position` across `node`'s rows in `csrs`, in turn.
fn target(csrs: &[&AnyCsr], node: DenseId, mut position: u64) -> Option<DenseId> {
    for csr in csrs {
        let degree = csr.degree(node);
        if position < degree {
            return csr.neighbours(node).nth(position as usize);
        }
        position -= degree;
    }
    None
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

    fn labels(
        graph: &GraphCsr,
        relations: RelationSelection<'_>,
    ) -> Result<Vec<i64>, ComputeError> {
        let memory = ComputeMemory::new(1 << 28);
        let found = strong_components(workers(), &memory, &mut Uninterrupted, graph, relations)?;
        Ok(found.values().to_vec())
    }

    /// Each node's label, the naive way: the smallest node that reaches it
    /// and that it reaches, from the transitive closure.
    fn naive(id_space: u64, edges: &Edges, selected: Option<&[usize]>) -> Vec<i64> {
        let n = id_space as usize;
        let mut reaches = vec![vec![false; n]; n];
        for (v, row) in reaches.iter_mut().enumerate() {
            row[v] = true;
        }
        for &(relation, s, t, _) in edges {
            if selected.is_none_or(|selected| selected.contains(&relation)) {
                reaches[s as usize][t as usize] = true;
            }
        }
        for k in 0..n {
            for i in 0..n {
                for j in 0..n {
                    reaches[i][j] |= reaches[i][k] && reaches[k][j];
                }
            }
        }
        (0..n)
            .map(|v| (0..n).find(|&u| reaches[u][v] && reaches[v][u]).unwrap_or(v) as i64)
            .collect()
    }

    #[test]
    fn cycles_are_labelled_by_their_smallest_node() {
        // 3 → 1 → 4 → 3 and 4 → 0 → 2 → 0, 5 → 5, 6 → 3.
        let edges: Edges = [(3, 1), (1, 4), (4, 3), (4, 0), (0, 2), (2, 0), (5, 5), (6, 3)]
            .into_iter()
            .map(|(s, t)| (0, s, t, 1.0))
            .collect();
        let graph = reference::graph(7, 1, &edges, false, false);
        assert_eq!(labels(&graph, RelationSelection::All), Ok(vec![0, 1, 0, 1, 1, 5, 6]));
    }

    #[test]
    fn a_path_of_a_million_nodes_needs_no_recursion() {
        let n: u64 = 1_000_000;
        let mut edges: Edges = (0..n - 1).map(|i| (0, i, i + 1, 1.0)).collect();
        edges.push((0, n - 1, 1, 1.0));
        let graph = reference::graph(n, 1, &edges, false, false);
        let found = labels(&graph, RelationSelection::All).unwrap();
        assert_eq!(found[0], 0);
        assert!(found[1..].iter().all(|&label| label == 1));
    }

    #[test]
    fn an_interrupt_cancels_the_search() {
        struct Stop;
        impl Interrupts for Stop {
            fn check(&mut self) -> Result<(), Cancelled> {
                Err(Cancelled)
            }
        }
        let graph = reference::graph(3, 1, &vec![(0, 0, 1, 1.0)], false, false);
        let memory = ComputeMemory::new(1 << 20);
        let result =
            strong_components(workers(), &memory, &mut Stop, &graph, RelationSelection::All);
        assert_eq!(result.map(|_| ()), Err(ComputeError::Cancelled));
    }

    #[test]
    fn a_cancellation_stops_a_long_search() {
        let n: u64 = 200_000;
        let edges: Edges = (0..n - 1).map(|i| (0, i, i + 1, 1.0)).collect();
        let graph = reference::graph(n, 1, &edges, false, false);
        let csrs = [graph.relations()[0].forward()];
        let memory = ComputeMemory::new(1 << 26);
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let mut found = vec![0; n as usize];
        let searched = label(&memory, &cancellation, &csrs, &mut found);
        assert_eq!(searched, Err(ComputeError::Cancelled));
    }

    #[test]
    fn the_lowlinks_and_stacks_are_charged_and_released() {
        let edges: Edges = (0..99).map(|i| (0, i, i + 1, 1.0)).collect();
        let graph = reference::graph(100, 1, &edges, false, false);
        let fits = |limit| {
            let memory = ComputeMemory::new(limit);
            let found = strong_components(
                workers(),
                &memory,
                &mut Uninterrupted,
                &graph,
                RelationSelection::All,
            );
            let fitted = found.is_ok();
            drop(found);
            assert_eq!(memory.in_use(), 0, "every charge is released");
            fitted
        };
        // The labels and lowlinks (2 × 800 bytes), and the two stacks,
        // which a path of 100 nodes fills.
        assert!(!fits(1600 + 100 * 8));
        assert!(fits(1600 + 128 * (8 + 16)));
    }

    #[test]
    fn held_memory_is_reclaimed_before_the_pool_charges() {
        let edges = vec![(0, 0, 1, 1.0), (0, 1, 0, 1.0)];
        let graph = reference::graph(3, 1, &edges, false, false);
        let held = 1 << 20;
        let memory = ComputeMemory::reclaiming(held, held, held, Box::new(Exact));
        let found = strong_components(
            workers(),
            &memory,
            &mut Uninterrupted,
            &graph,
            RelationSelection::All,
        );
        assert_eq!(found.map(|found| found.values().to_vec()), Ok(vec![0, 0, 2]));
    }

    type Scenario = (u64, usize, Edges, Option<Vec<usize>>, (bool, bool));

    fn scenarios() -> impl Strategy<Value = Scenario> {
        (1u64..12, 1usize..4).prop_flat_map(|(id_space, relations)| {
            let edge = (0..relations, 0..id_space, 0..id_space, Just(1.0));
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
        fn labels_match_the_transitive_closure((id_space, relations, edges, selected, (wide, store_merged)) in scenarios()) {
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
            prop_assert_eq!(labels(&graph, selection), Ok(naive(id_space, &edges, selected.as_deref())));
        }
    }
}
