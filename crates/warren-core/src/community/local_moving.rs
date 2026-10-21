//! Local moving (DESIGN.md §6.7): Louvain's first phase (Blondel et al.,
//! "Fast unfolding of communities in large networks", 2008), each node
//! moving to the neighbouring community that most increases modularity,
//! sweep after sweep until no node moves.
//!
//! Sequential local moving is inherently ordered: a node decides against
//! the moves of every node before it. This kernel makes the sweep parallel
//! and still deterministic with Grappolo's colouring (Lu, Halappanavar and
//! Kalyanaraman, "Parallel heuristics for scalable community detection",
//! 2015): the nodes are coloured greedily so that no two neighbours share
//! a colour, and a sweep takes the colours one at a time. The nodes of one
//! colour are not adjacent, so each one's weights to the neighbouring
//! communities do not change while the others decide: they decide in
//! parallel against the state at the start of their colour. Their moves are
//! then applied one by one in a fixed order, each checked against the
//! communities' degree sums as they then are (only those can have changed)
//! and kept only if it still increases modularity. So modularity rises with
//! every move applied, the run always ends, and the result is the same for
//! any thread count: decisions are a function of the state, and the order
//! of application is fixed. The seed orders the nodes for the colouring
//! (a counter-based hash of seed and node), and so the sweep.

use std::mem::size_of;

use rayon::prelude::*;

use super::random::priority;
use super::{modularity, CommunityError, Undirected};
use crate::compute::{ComputeError, ComputeMemory, MemoryCharge, Vector, Workers};
use crate::graph::{DenseId, Tombstones};
use crate::interrupts::{Cancellation, Interrupts};

/// A colour with at least this many nodes decides in parallel tasks of
/// this many nodes; a smaller one on the calling thread. Small under test.
const NODES_PER_TASK: usize = if cfg!(test) { 2 } else { 2048 };

/// How a local moving runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalMoving {
    /// The resolution `γ`: above 1 favours smaller communities.
    pub resolution: f64,
    /// Orders the nodes, and so the sweep.
    pub seed: u64,
    /// The most sweeps over the nodes.
    pub max_sweeps: u32,
    /// A sweep that raises modularity by no more than this is the last.
    pub tolerance: f64,
}

/// The partition a local moving ends with.
#[derive(Debug)]
pub struct Moved {
    /// Each live node's community, labelled by the smallest key of its
    /// members (tombstoned nodes hold 0).
    pub labels: Vector<i64>,
    /// The number of communities of live nodes.
    pub communities: u64,
    /// The sweeps run.
    pub sweeps: u32,
    /// The moves applied.
    pub moves: u64,
    /// The partition's modularity, as the moves updated it.
    pub modularity: f64,
}

/// Moves the live nodes of `graph` between communities, starting from the
/// partition `initial` labels (each node alone if `None`), until a sweep
/// moves no node, raises modularity by no more than `options.tolerance`,
/// or `options.max_sweeps` have run. Communities are labelled by the
/// smallest of their members' `keys` (dense ids if `None`).
#[allow(clippy::too_many_arguments)] // a kernel's inputs, as `refine` takes them
pub fn local_moving<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &Undirected,
    tombstones: &Tombstones,
    initial: Option<&[i64]>,
    keys: Option<&[i64]>,
    options: LocalMoving,
) -> Result<Moved, CommunityError> {
    let nodes = graph.id_space();
    for vector in [initial, keys].into_iter().flatten() {
        if vector.len() as u64 != nodes {
            return Err(ComputeError::IdSpaceMismatch {
                expected: nodes,
                found: vector.len() as u64,
            }
            .into());
        }
    }
    let mut labels = Vector::filled(memory, nodes, 1, 0i64)?;
    memory.reclaim_for(State::bytes(nodes, graph));
    let outcome = workers.run(interrupts, |cancellation| {
        Ok(run(
            memory,
            cancellation,
            graph,
            tombstones,
            initial,
            keys,
            options,
            labels.values_mut(),
        ))
    })??;
    let (communities, sweeps, moves, modularity) = outcome;
    Ok(Moved { labels, communities, sweeps, moves, modularity })
}

/// [`local_moving`]'s work on the pool: writes the labels and returns the
/// community count, sweeps, moves and modularity.
#[allow(clippy::too_many_arguments)] // the kernel's inputs, passed through from one call
fn run(
    memory: &ComputeMemory,
    cancellation: &Cancellation,
    graph: &Undirected,
    tombstones: &Tombstones,
    initial: Option<&[i64]>,
    keys: Option<&[i64]>,
    options: LocalMoving,
    labels: &mut [i64],
) -> Result<(u64, u32, u64, f64), CommunityError> {
    let mut state = State::new(memory, graph, initial)?;
    let classes = Colouring::new(memory, cancellation, graph, options.seed)?;
    let mut q = state.modularity(memory, graph, options.resolution)?;
    let (mut sweeps, mut moves) = (0, 0);
    while sweeps < options.max_sweeps {
        sweeps += 1;
        let before = q;
        let mut moved = 0;
        for class in classes.each() {
            if cancellation.is_cancelled() {
                return Err(ComputeError::Cancelled.into());
            }
            let decisions = state.decide(graph, class, options.resolution);
            for decision in decisions.into_iter().flatten() {
                if let Some(gain) = state.apply(graph, decision, options.resolution) {
                    q += gain;
                    moved += 1;
                }
            }
        }
        moves += moved;
        if moved == 0 || q - before <= options.tolerance {
            break;
        }
    }
    let communities = state.label(tombstones, keys, labels, memory)?;
    Ok((communities, sweeps, moves, q))
}

/// A node's best move: to `target`, with its weights to that community
/// and to its own (itself left out).
#[derive(Debug, Clone, Copy)]
struct Decision {
    node: DenseId,
    target: DenseId,
    to_target: f64,
    to_own: f64,
}

/// Each node's community (a node standing for it) and each community's
/// degree sum.
struct State {
    community: Vec<DenseId>,
    sums: Vec<f64>,
    _charges: [MemoryCharge; 2],
}

impl State {
    /// The bytes the state and the colouring hold for `nodes` nodes, and the
    /// decisions' scratch at its largest.
    fn bytes(nodes: u64, graph: &Undirected) -> u64 {
        let longest = (0..nodes).map(|node| graph.row(node).len() as u64).max().unwrap_or(0);
        let per_node = (size_of::<DenseId>() + size_of::<f64>()) as u64 + Colouring::BYTES_PER_NODE;
        nodes.saturating_mul(per_node).saturating_add(longest.saturating_mul(16 * 64))
    }

    fn new(
        memory: &ComputeMemory,
        graph: &Undirected,
        initial: Option<&[i64]>,
    ) -> Result<Self, CommunityError> {
        let nodes = graph.id_space() as usize;
        let (mut community, community_charge) = memory.filled(nodes, 0 as DenseId)?;
        match initial {
            None => community.iter_mut().enumerate().for_each(|(node, c)| *c = node as DenseId),
            Some(labels) => {
                // Each community is represented by its member of least dense id.
                let (mut order, _order_charge) = memory.filled(nodes, 0 as DenseId)?;
                order.iter_mut().enumerate().for_each(|(node, o)| *o = node as DenseId);
                order.sort_by_key(|&node| (labels[node as usize], node));
                let mut first = 0;
                for (at, &node) in order.iter().enumerate() {
                    if at == 0 || labels[order[at - 1] as usize] != labels[node as usize] {
                        first = node;
                    }
                    community[node as usize] = first;
                }
            }
        }
        let (mut sums, sums_charge) = memory.filled(nodes, 0.0)?;
        for (node, &c) in community.iter().enumerate() {
            sums[c as usize] += graph.degree(node as DenseId);
        }
        Ok(Self { community, sums, _charges: [community_charge, sums_charge] })
    }

    fn modularity(
        &self,
        memory: &ComputeMemory,
        graph: &Undirected,
        resolution: f64,
    ) -> Result<f64, CommunityError> {
        let (mut labels, _labels_charge) = memory.filled(self.community.len(), 0i64)?;
        labels.iter_mut().zip(&self.community).for_each(|(label, &c)| *label = c as i64);
        modularity(memory, graph, &labels, resolution)
    }

    /// The best move of each node of `class`, decided against the current
    /// state, in parallel when the class is large; in `class` order.
    fn decide(
        &self,
        graph: &Undirected,
        class: &[DenseId],
        resolution: f64,
    ) -> Vec<Option<Decision>> {
        let decide_chunk = |chunk: &[DenseId]| {
            let mut scratch = Vec::new();
            chunk
                .iter()
                .map(|&node| self.best(graph, node, resolution, &mut scratch))
                .collect::<Vec<_>>()
        };
        if class.len() < NODES_PER_TASK {
            return decide_chunk(class);
        }
        class.par_chunks(NODES_PER_TASK).flat_map_iter(decide_chunk).collect()
    }

    /// Node `node`'s best move, if moving raises modularity: to the
    /// neighbouring community of greatest gain, the one represented by the
    /// least dense id on a tie.
    fn best(
        &self,
        graph: &Undirected,
        node: DenseId,
        resolution: f64,
        scratch: &mut Vec<(DenseId, f64)>,
    ) -> Option<Decision> {
        let total = graph.total();
        let degree = graph.degree(node);
        if total == 0.0 || degree == 0.0 {
            return None;
        }
        scratch.clear();
        scratch.extend(
            graph
                .row(node)
                .iter()
                .filter(|&&(neighbour, _)| neighbour != node)
                .map(|&(neighbour, weight)| (self.community[neighbour as usize], weight)),
        );
        scratch.sort_by_key(|&(community, _)| community);
        let own = self.community[node as usize];
        let gain = |to: f64, sum: f64| to - resolution * degree * sum / total;
        let mut to_own = 0.0;
        let mut best: Option<(DenseId, f64, f64)> = None;
        let mut at = 0;
        while at < scratch.len() {
            let community = scratch[at].0;
            let mut to = 0.0;
            while at < scratch.len() && scratch[at].0 == community {
                to += scratch[at].1;
                at += 1;
            }
            if community == own {
                to_own = to;
                continue;
            }
            let candidate = gain(to, self.sums[community as usize]);
            if best.is_none_or(|(_, _, best_gain)| candidate > best_gain) {
                best = Some((community, to, candidate));
            }
        }
        let stay = gain(to_own, self.sums[own as usize] - degree);
        let (target, to_target, _) = best.filter(|&(_, _, best_gain)| best_gain > stay)?;
        Some(Decision { node, target, to_target, to_own })
    }

    /// Applies `decision` if it still raises modularity against the
    /// current degree sums, and returns the rise.
    fn apply(&mut self, graph: &Undirected, decision: Decision, resolution: f64) -> Option<f64> {
        let Decision { node, target, to_target, to_own } = decision;
        let (total, degree) = (graph.total(), graph.degree(node));
        let own = self.community[node as usize];
        let stay = to_own - resolution * degree * (self.sums[own as usize] - degree) / total;
        let moved = to_target - resolution * degree * self.sums[target as usize] / total;
        if moved <= stay {
            return None;
        }
        self.sums[own as usize] -= degree;
        self.sums[target as usize] += degree;
        self.community[node as usize] = target;
        Some(2.0 * (moved - stay) / total)
    }

    /// Writes each live node's label, the least key of its community's
    /// members, and returns the number of communities.
    fn label(
        &self,
        tombstones: &Tombstones,
        keys: Option<&[i64]>,
        labels: &mut [i64],
        memory: &ComputeMemory,
    ) -> Result<u64, CommunityError> {
        let key = |node: usize| keys.map_or(node as i64, |keys| keys[node]);
        let (mut least, _least_charge) = memory.filled(labels.len(), None::<i64>)?;
        for (node, &community) in self.community.iter().enumerate() {
            if !tombstones.contains(node as DenseId) {
                let slot = &mut least[community as usize];
                *slot = Some(slot.map_or(key(node), |least| least.min(key(node))));
            }
        }
        for (node, label) in labels.iter_mut().enumerate() {
            if !tombstones.contains(node as DenseId) {
                *label = least[self.community[node] as usize].unwrap_or_default();
            }
        }
        Ok(least.iter().filter(|slot| slot.is_some()).count() as u64)
    }
}

/// The nodes with an edge, grouped by colour (no two neighbours share
/// one), each colour in the seeded order the colouring took them.
struct Colouring {
    starts: Vec<usize>,
    nodes: Vec<DenseId>,
    _charge: MemoryCharge,
}

impl Colouring {
    /// The bytes per node it holds while it colours.
    const BYTES_PER_NODE: u64 = 3 * size_of::<u64>() as u64;

    fn new(
        memory: &ComputeMemory,
        cancellation: &Cancellation,
        graph: &Undirected,
        seed: u64,
    ) -> Result<Self, CommunityError> {
        let nodes = graph.id_space() as usize;
        let charge = memory.charge(Self::BYTES_PER_NODE.saturating_mul(nodes as u64))?;
        let mut order: Vec<DenseId> =
            (0..nodes as DenseId).filter(|&node| !graph.row(node).is_empty()).collect();
        order.sort_by_key(|&node| (priority(seed, node), node));
        let mut colour = vec![usize::MAX; nodes];
        let mut taken: Vec<DenseId> = Vec::new();
        let mut colours = 0;
        for (at, &node) in order.iter().enumerate() {
            if at % 65_536 == 0 && cancellation.is_cancelled() {
                return Err(ComputeError::Cancelled.into());
            }
            for &(neighbour, _) in graph.row(node) {
                let c = colour[neighbour as usize];
                if c != usize::MAX && neighbour != node {
                    if c >= taken.len() {
                        taken.resize(c + 1, DenseId::MAX);
                    }
                    taken[c] = node;
                }
            }
            let free = (0..).find(|&c| taken.get(c).is_none_or(|&by| by != node)).unwrap_or(0);
            colour[node as usize] = free;
            colours = colours.max(free + 1);
        }
        let mut starts = vec![0usize; colours + 1];
        for &node in &order {
            starts[colour[node as usize] + 1] += 1;
        }
        for c in 0..colours {
            starts[c + 1] += starts[c];
        }
        let mut next = starts.clone();
        let mut grouped = vec![0 as DenseId; order.len()];
        for &node in &order {
            let slot = &mut next[colour[node as usize]];
            grouped[*slot] = node;
            *slot += 1;
        }
        Ok(Self { starts, nodes: grouped, _charge: charge })
    }

    /// Each colour's nodes, colour by colour.
    fn each(&self) -> impl Iterator<Item = &[DenseId]> {
        self.starts.windows(2).map(|range| &self.nodes[range[0]..range[1]])
    }
}

// Not under Miri: the compute pool's work stealing runs through
// crossbeam-epoch, which Miri's Stacked Borrows model rejects.
#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::sync::OnceLock;

    use super::*;
    use crate::compute::reference::{self, Edges};
    use crate::interrupts::Uninterrupted;

    fn workers() -> &'static Workers {
        static WORKERS: OnceLock<Workers> = OnceLock::new();
        WORKERS.get_or_init(|| Workers::new(3).unwrap())
    }

    pub(super) fn undirected(n: u64, edges: &Edges) -> Undirected {
        let mut both = edges.clone();
        both.extend(edges.iter().filter(|e| e.1 != e.2).map(|&(r, s, t, w)| (r, t, s, w)));
        let graph = reference::graph(n, 1, &both, true, false);
        Undirected::new(&ComputeMemory::new(u64::MAX), &Cancellation::new(), &graph, true, false)
            .unwrap()
    }

    const OPTIONS: LocalMoving =
        LocalMoving { resolution: 1.0, seed: 0, max_sweeps: 100, tolerance: 0.0 };

    fn moved(
        graph: &Undirected,
        workers: &Workers,
        initial: Option<&[i64]>,
        options: LocalMoving,
    ) -> Moved {
        let tombstones = Tombstones::new(graph.id_space()).unwrap();
        let memory = ComputeMemory::new(u64::MAX);
        local_moving(
            workers,
            &memory,
            &mut Uninterrupted,
            graph,
            &tombstones,
            initial,
            None,
            options,
        )
        .unwrap()
    }

    /// Two 4-cliques joined by one edge.
    fn two_cliques() -> Undirected {
        let mut edges = Edges::new();
        for base in [0, 4] {
            for a in 0..4 {
                for b in a + 1..4 {
                    edges.push((0, base + a, base + b, 1.0));
                }
            }
        }
        edges.push((0, 3, 4, 1.0));
        undirected(8, &edges)
    }

    #[test]
    fn two_cliques_become_two_communities() {
        let graph = two_cliques();
        let found = moved(&graph, workers(), None, OPTIONS);
        assert_eq!(found.labels.values(), [0, 0, 0, 0, 4, 4, 4, 4]);
        assert_eq!(found.communities, 2);
        let exact =
            modularity(&ComputeMemory::new(u64::MAX), &graph, found.labels.values(), 1.0).unwrap();
        assert!((found.modularity - exact).abs() < 1e-12);
    }

    #[test]
    fn the_result_is_the_same_on_any_number_of_threads() {
        let edges: Edges =
            (0..300u64).map(|i| (0, i % 61, (i * i * 7 + 3) % 61, 1.0 + (i % 3) as f64)).collect();
        let graph = undirected(61, &edges);
        for seed in [0, 1, 2] {
            let options = LocalMoving { seed, ..OPTIONS };
            let one = moved(&graph, &Workers::new(1).unwrap(), None, options);
            let many = moved(&graph, workers(), None, options);
            assert_eq!(one.labels.values(), many.labels.values(), "seed {seed}");
            assert_eq!(one.modularity.to_bits(), many.modularity.to_bits());
        }
    }

    #[test]
    fn every_move_raises_modularity() {
        let edges: Edges = (0..200u64).map(|i| (0, i % 40, (i * 13 + 5) % 40, 1.0)).collect();
        let graph = undirected(40, &edges);
        let singletons: Vec<i64> = (0..40).collect();
        let before = modularity(&ComputeMemory::new(u64::MAX), &graph, &singletons, 1.0).unwrap();
        let found = moved(&graph, workers(), None, OPTIONS);
        let after =
            modularity(&ComputeMemory::new(u64::MAX), &graph, found.labels.values(), 1.0).unwrap();
        assert!(after > before);
        assert!((found.modularity - after).abs() < 1e-12);
        assert!(found.moves > 0);
    }

    #[test]
    fn an_initial_partition_is_where_it_starts() {
        let graph = two_cliques();
        let initial = [7, 7, 7, 7, 9, 9, 9, 9];
        let found = moved(&graph, workers(), Some(&initial), OPTIONS);
        assert_eq!(found.moves, 0, "already optimal");
        assert_eq!(found.labels.values(), [0, 0, 0, 0, 4, 4, 4, 4]);
    }

    #[test]
    fn communities_are_labelled_by_their_least_key() {
        let graph = two_cliques();
        let keys = [80, 81, 82, 83, 14, 15, 16, 17];
        let tombstones = Tombstones::new(8).unwrap();
        let memory = ComputeMemory::new(u64::MAX);
        let found = local_moving(
            workers(),
            &memory,
            &mut Uninterrupted,
            &graph,
            &tombstones,
            None,
            Some(&keys),
            OPTIONS,
        )
        .unwrap();
        assert_eq!(found.labels.values(), [80, 80, 80, 80, 14, 14, 14, 14]);
    }

    #[test]
    fn a_higher_resolution_keeps_smaller_communities() {
        let graph = two_cliques();
        let options = LocalMoving { resolution: 20.0, ..OPTIONS };
        let found = moved(&graph, workers(), None, options);
        assert!(found.communities > 2, "{:?}", found.labels.values());
    }

    #[test]
    fn nodes_without_edges_stay_alone() {
        let graph = undirected(3, &vec![(0, 0, 1, 1.0)]);
        let found = moved(&graph, workers(), None, OPTIONS);
        assert_eq!(found.labels.values(), [0, 0, 2]);
        assert_eq!(found.communities, 2);
    }
}
