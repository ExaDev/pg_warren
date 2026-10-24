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
//!
//! After the first sweep, a sweep decides only for the nodes a move has
//! woken, the neighbours of a moved node outside the community it joined,
//! as Traag et al.'s fast local moving queues them, so the long tail of
//! sweeps that move a few nodes costs about those nodes' rows rather than
//! the whole graph. The moving ends when no node is woken. A move also
//! changes the gains of nodes that are not its neighbours (through their
//! communities' degree sums), which no wake covers, so a node may end
//! with a move that would still raise modularity, as in Traag et al.'s;
//! sweeping every node until none moves found partitions of about the
//! same modularity at many times the cost (DESIGN.md §6.7).

use std::mem::size_of;

use rayon::prelude::*;

use super::modularity::GROUPING_BYTES_PER_NODE;
use super::random::priority;
use super::{modularity, CommunityError, Undirected};
use crate::compute::tally::{Reporting, Tally};
use crate::compute::{ComputeError, ComputeMemory, MemoryCharge, Vector, Workers};
use crate::graph::{DenseId, Tombstones};
use crate::interrupts::{Cancellation, Interrupts};

/// A colour with at least this many nodes decides in parallel tasks of
/// this many nodes; a smaller one on the calling thread. Small under test.
const NODES_PER_TASK: usize = if cfg!(test) { 2 } else { 2048 };

/// The nodes of a colour decided at once, against one state: small under
/// test, so tests cross blocks.
const NODES_PER_BLOCK: usize = if cfg!(test) { 5 } else { 1 << 16 };

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
    memory.reclaim_for(State::bytes(nodes, graph, workers.threads()));
    let tally = Tally::new(Some(u64::from(options.max_sweeps)));
    let mut reporting = Reporting::start(interrupts, &tally, "moving", ["moved"]);
    let outcome = workers.run(&mut reporting, |cancellation| {
        let job = Job { cancellation, tally: &tally };
        Ok(run(memory, job, graph, tombstones, initial, keys, options, labels.values_mut()?))
    })??;
    reporting.report();
    let (communities, sweeps, moves, modularity) = outcome;
    Ok(Moved { labels, communities, sweeps, moves, modularity })
}

/// What [`local_moving`]'s job on the pool polls and reports to: the sweeps
/// done, and the nodes moved.
#[derive(Clone, Copy)]
struct Job<'a> {
    cancellation: &'a Cancellation,
    tally: &'a Tally<1>,
}

/// [`local_moving`]'s work on the pool: writes the labels and returns the
/// community count, sweeps, moves and modularity.
#[allow(clippy::too_many_arguments)] // the kernel's inputs, passed through from one call
fn run(
    memory: &ComputeMemory,
    Job { cancellation, tally }: Job<'_>,
    graph: &Undirected,
    tombstones: &Tombstones,
    initial: Option<&[i64]>,
    keys: Option<&[i64]>,
    options: LocalMoving,
    labels: &mut [i64],
) -> Result<(u64, u32, u64, f64), CommunityError> {
    let mut state = State::new(memory, graph, initial)?;
    let mut q = state.modularity(memory, graph, options.resolution)?;
    let classes = Colouring::new(memory, cancellation, graph, options.seed)?;
    let mut sweep = Sweep::new(memory, graph.id_space() as usize)?;
    let (mut sweeps, mut moves) = (0, 0);
    while sweeps < options.max_sweeps {
        sweeps += 1;
        let before = q;
        let mut moved = 0;
        for class in classes.each() {
            for block in class.chunks(NODES_PER_BLOCK) {
                if cancellation.is_cancelled() {
                    return Err(ComputeError::Cancelled.into());
                }
                let count = sweep.take(block);
                let (pending, decisions) = sweep.batch(count);
                state.decide(memory, graph, pending, decisions, options.resolution)?;
                for at in 0..count {
                    let Some(decision) = sweep.decisions[at] else { continue };
                    if let Some(gain) = state.apply(graph, decision, options.resolution) {
                        q += gain;
                        moved += 1;
                        sweep.wake_neighbours(graph, &state, decision);
                    }
                }
            }
        }
        moves += moved;
        tally.set_done(u64::from(sweeps));
        tally.set_detail(0, moves as f64);
        // A sweep that moves none leaves no node woken.
        if moved == 0 || q - before <= options.tolerance {
            break;
        }
    }
    drop(sweep);
    let communities = state.label(tombstones, keys, labels, memory)?;
    Ok((communities, sweeps, moves, q))
}

/// Which nodes a sweep decides for. The first sweep takes every node;
/// later ones only the nodes woken since they last decided: the
/// neighbours of a node that moved, outside the community it joined (the
/// queue of Traag et al.'s fast local moving).
struct Sweep {
    woken: Vec<bool>,
    pending: Vec<DenseId>,
    decisions: Vec<Option<Decision>>,
    _charges: [MemoryCharge; 3],
}

impl Sweep {
    /// The bytes it holds for `nodes` nodes: a flag a node, and a block's
    /// nodes and decisions.
    fn bytes(nodes: u64) -> u64 {
        let block = (NODES_PER_BLOCK as u64).min(nodes);
        let per_block = (size_of::<DenseId>() + size_of::<Option<Decision>>()) as u64;
        nodes.saturating_mul(size_of::<bool>() as u64).saturating_add(block * per_block)
    }

    fn new(memory: &ComputeMemory, nodes: usize) -> Result<Self, CommunityError> {
        let block = NODES_PER_BLOCK.min(nodes);
        let (woken, woken_charge) = memory.filled(nodes, true)?;
        let (pending, pending_charge) = memory.filled(block, 0 as DenseId)?;
        let (decisions, decisions_charge) = memory.filled(block, None)?;
        Ok(Self {
            woken,
            pending,
            decisions,
            _charges: [woken_charge, pending_charge, decisions_charge],
        })
    }

    /// Puts the woken nodes of `block` first in `pending`, each back to
    /// sleep, and returns how many. Every node starts woken.
    fn take(&mut self, block: &[DenseId]) -> usize {
        let mut count = 0;
        for &node in block {
            if self.woken[node as usize] {
                self.woken[node as usize] = false;
                self.pending[count] = node;
                count += 1;
            }
        }
        count
    }

    /// The first `count` pending nodes, and room for their decisions.
    fn batch(&mut self, count: usize) -> (&[DenseId], &mut [Option<Decision>]) {
        (&self.pending[..count], &mut self.decisions[..count])
    }

    /// Wakes the neighbours of `decision`'s node outside the community it
    /// joined.
    fn wake_neighbours(&mut self, graph: &Undirected, state: &State, decision: Decision) {
        for &(neighbour, _) in graph.row(decision.node) {
            if state.community[neighbour as usize] != decision.target {
                self.woken[neighbour as usize] = true;
            }
        }
    }
}

/// A task's scratch for one row's communities and weights at a time,
/// charged to the budget as it grows: at most the longest row.
struct Scratch {
    rows: Vec<(DenseId, f64)>,
    charge: MemoryCharge,
}

impl Scratch {
    /// The bytes per row entry it holds.
    const BYTES_PER_ENTRY: u64 = size_of::<(DenseId, f64)>() as u64;

    fn new(memory: &ComputeMemory) -> Result<Self, CommunityError> {
        Ok(Self { rows: Vec::new(), charge: memory.charge(0)? })
    }

    /// The scratch, empty, with room for `len` entries.
    fn room(&mut self, len: usize) -> Result<&mut Vec<(DenseId, f64)>, CommunityError> {
        self.rows.clear();
        let capacity = self.rows.capacity();
        if len > capacity {
            let bytes = (len - capacity) as u64 * Self::BYTES_PER_ENTRY;
            self.charge.grow(bytes)?;
            self.rows.try_reserve_exact(len).map_err(|_| ComputeError::OutOfMemory { bytes })?;
        }
        Ok(&mut self.rows)
    }
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
    /// The most [`local_moving`] charges at once on the pool for `nodes`
    /// nodes on `threads` threads: per node, the state (16 B) beside the
    /// starting modularity's labels and grouping (40 B, before the
    /// colouring exists), the colouring as it colours (24 B), or the
    /// colouring it keeps (8 B) with the sweeps' flags (1 B) or, once
    /// those are gone, the labelling's least keys (16 B); and the
    /// colouring's per-colour arrays and the sweeps' block, and each
    /// thread's row scratch, all at their largest.
    fn bytes(nodes: u64, graph: &Undirected, threads: usize) -> u64 {
        let longest = (0..nodes).map(|node| graph.row(node).len() as u64).max().unwrap_or(0);
        let state = (size_of::<DenseId>() + size_of::<f64>()) as u64;
        let modularity = size_of::<i64>() as u64 + GROUPING_BYTES_PER_NODE;
        let kept = size_of::<DenseId>() as u64;
        let later = (kept + size_of::<bool>() as u64).max(kept + 16);
        let per_node = state + modularity.max(Colouring::BYTES_PER_NODE).max(later);
        nodes
            .saturating_mul(per_node)
            .saturating_add(Colouring::bytes_per_colouring(longest))
            .saturating_add(Sweep::bytes(nodes) - nodes)
            .saturating_add((threads as u64).saturating_mul(longest) * Scratch::BYTES_PER_ENTRY)
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
                order.sort_unstable_by_key(|&node| (labels[node as usize], node));
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

    /// The best move of each node of `nodes` into `decisions`, decided
    /// against the current state, in parallel when there are many. Each
    /// task's row scratch is charged to `memory` as it grows.
    fn decide(
        &self,
        memory: &ComputeMemory,
        graph: &Undirected,
        nodes: &[DenseId],
        decisions: &mut [Option<Decision>],
        resolution: f64,
    ) -> Result<(), CommunityError> {
        let decide_chunk = |(nodes, decisions): (&[DenseId], &mut [Option<Decision>])| {
            let mut scratch = Scratch::new(memory)?;
            for (&node, decision) in nodes.iter().zip(decisions) {
                *decision = self.best(graph, node, resolution, &mut scratch)?;
            }
            Ok::<_, CommunityError>(())
        };
        if nodes.len() < NODES_PER_TASK {
            return decide_chunk((nodes, decisions));
        }
        nodes
            .par_chunks(NODES_PER_TASK)
            .zip(decisions.par_chunks_mut(NODES_PER_TASK))
            .try_for_each(decide_chunk)
    }

    /// Node `node`'s best move, if moving raises modularity: to the
    /// neighbouring community of greatest gain, the one represented by the
    /// least dense id on a tie.
    fn best(
        &self,
        graph: &Undirected,
        node: DenseId,
        resolution: f64,
        scratch: &mut Scratch,
    ) -> Result<Option<Decision>, CommunityError> {
        let total = graph.total();
        let degree = graph.degree(node);
        if total == 0.0 || degree == 0.0 {
            return Ok(None);
        }
        let scratch = scratch.room(graph.row(node).len())?;
        scratch.extend(
            graph
                .row(node)
                .iter()
                .filter(|&&(neighbour, _)| neighbour != node)
                .map(|&(neighbour, weight)| (self.community[neighbour as usize], weight)),
        );
        scratch.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
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
        Ok(best
            .filter(|&(_, _, best_gain)| best_gain > stay)
            .map(|(target, to_target, _)| Decision { node, target, to_target, to_own }))
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
    _charges: [MemoryCharge; 2],
}

impl Colouring {
    /// The bytes per node it holds while it colours: the order, each
    /// node's colour and the nodes grouped by colour, which it keeps.
    const BYTES_PER_NODE: u64 = 3 * size_of::<u64>() as u64;

    /// The bytes of its per-colour arrays at most, for rows of at most
    /// `longest` entries: there are at most `longest + 1` colours.
    fn bytes_per_colouring(longest: u64) -> u64 {
        3 * size_of::<u64>() as u64 * longest.saturating_add(2)
    }

    fn new(
        memory: &ComputeMemory,
        cancellation: &Cancellation,
        graph: &Undirected,
        seed: u64,
    ) -> Result<Self, CommunityError> {
        let nodes = graph.id_space() as usize;
        let (mut order, _order_charge) = memory.filled(nodes, 0 as DenseId)?;
        let mut len = 0;
        for node in (0..nodes as DenseId).filter(|&node| !graph.row(node).is_empty()) {
            order[len] = node;
            len += 1;
        }
        order.truncate(len);
        order.sort_unstable_by_key(|&node| (priority(seed, node), node));
        let longest = order.iter().map(|&node| graph.row(node).len()).max().unwrap_or(0);
        let (mut colour, _colour_charge) = memory.filled(nodes, usize::MAX)?;
        let (mut taken, _taken_charge) = memory.filled(longest + 1, DenseId::MAX)?;
        let mut colours = 0;
        for (at, &node) in order.iter().enumerate() {
            if at % 65_536 == 0 && cancellation.is_cancelled() {
                return Err(ComputeError::Cancelled.into());
            }
            for &(neighbour, _) in graph.row(node) {
                let c = colour[neighbour as usize];
                if c != usize::MAX && neighbour != node {
                    taken[c] = node;
                }
            }
            // A row of d entries takes at most d colours, so one of the
            // first d + 1 is free.
            let free = (0..=longest).find(|&c| taken[c] != node).unwrap_or(0);
            colour[node as usize] = free;
            colours = colours.max(free + 1);
        }
        let (mut starts, starts_charge) = memory.filled(colours + 1, 0usize)?;
        for &node in &order {
            starts[colour[node as usize] + 1] += 1;
        }
        for c in 0..colours {
            starts[c + 1] += starts[c];
        }
        let (mut next, _next_charge) = memory.filled(colours + 1, 0usize)?;
        next.copy_from_slice(&starts);
        let (mut grouped, grouped_charge) = memory.filled(order.len(), 0 as DenseId)?;
        for &node in &order {
            let slot = &mut next[colour[node as usize]];
            grouped[*slot] = node;
            *slot += 1;
        }
        Ok(Self { starts, nodes: grouped, _charges: [starts_charge, grouped_charge] })
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

    use proptest::prelude::*;

    use super::*;
    use crate::community::Weighting;
    use crate::compute::reference::{self, Edges};
    use crate::interrupts::Uninterrupted;
    use crate::progress::recorded::Recorded;

    fn workers() -> &'static Workers {
        static WORKERS: OnceLock<Workers> = OnceLock::new();
        WORKERS.get_or_init(|| Workers::new(3).unwrap())
    }

    pub(super) fn undirected(n: u64, edges: &Edges) -> Undirected {
        let mut both = edges.clone();
        both.extend(edges.iter().filter(|e| e.1 != e.2).map(|&(r, s, t, w)| (r, t, s, w)));
        let graph = reference::graph(n, 1, &both, true, false);
        Undirected::new(
            &ComputeMemory::new(u64::MAX),
            &Cancellation::new(),
            &graph,
            Weighting { weighted: true, per_relation: false },
            false,
        )
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

    proptest! {
        /// The modularity it reports is its partition's, and no less than
        /// the singletons'.
        #[test]
        fn its_modularity_is_its_partitions_and_never_falls(
            (n, edges) in (2u64..16).prop_flat_map(|n| (
                Just(n),
                prop::collection::vec((0..n, 0..n, 1..4u32), 0..50),
            )),
            seed in any::<u64>(),
        ) {
            let edges: Edges = edges.into_iter().map(|(s, t, w)| (0, s, t, f64::from(w))).collect();
            let graph = undirected(n, &edges);
            let found = moved(&graph, workers(), None, LocalMoving { seed, ..OPTIONS });
            let memory = ComputeMemory::new(u64::MAX);
            let q = modularity(&memory, &graph, found.labels.values(), 1.0).unwrap();
            let singletons: Vec<i64> = (0..n as i64).collect();
            let start = modularity(&memory, &graph, &singletons, 1.0).unwrap();
            prop_assert!((found.modularity - q).abs() < 1e-12, "{} != {q}", found.modularity);
            prop_assert!(q >= start - 1e-12, "{q} < {start}");
        }
    }

    proptest! {
        /// It reports its sweeps of `max_sweeps` and the nodes moved, and
        /// reporting changes no community label.
        #[test]
        fn it_reports_its_sweeps_and_moves_and_reporting_changes_nothing(
            (n, edges) in (2u64..40).prop_flat_map(|n| (
                Just(n),
                prop::collection::vec((0..n, 0..n, 1..4u32), 0..120),
            )),
            seed in any::<u64>(),
            max_sweeps in 1u32..6,
        ) {
            let edges: Edges = edges.into_iter().map(|(s, t, w)| (0, s, t, f64::from(w))).collect();
            let graph = undirected(n, &edges);
            let options = LocalMoving { seed, max_sweeps, ..OPTIONS };
            let tombstones = Tombstones::new(n).unwrap();
            let memory = ComputeMemory::new(u64::MAX);
            let mut recorded = Recorded::default();
            let found = local_moving(
                workers(), &memory, &mut recorded, &graph, &tombstones, None, None, options,
            )
            .unwrap();
            let unreported = moved(&graph, workers(), None, options);
            prop_assert_eq!(found.labels.values(), unreported.labels.values());
            prop_assert_eq!(recorded.phases(), vec![("moving", Some(u64::from(max_sweeps)))]);
            prop_assert_eq!(recorded.checked_done(), vec![u64::from(found.sweeps)]);
            prop_assert_eq!(recorded.last_detail("moved"), Some(found.moves as f64));
        }
    }

    #[test]
    fn it_reclaims_ahead_for_everything_it_charges_on_the_pool() {
        // A hub of 5,000 leaves, two of them joined, and a path: the row
        // scratch is the hub's row on each thread.
        let mut edges: Edges = (1..5_001u64).map(|leaf| (0, 0, leaf, 1.0)).collect();
        edges.extend((5_001..5_100u64).map(|n| (0, n, n + 1, 1.0)));
        edges.push((0, 1, 2, 1.0));
        let graph = undirected(5_101, &edges);
        let tombstones = Tombstones::new(graph.id_space()).unwrap();
        let memory = crate::compute::memory::fully_held();
        let found = local_moving(
            workers(),
            &memory,
            &mut Uninterrupted,
            &graph,
            &tombstones,
            None,
            None,
            OPTIONS,
        );
        assert!(found.is_ok(), "{found:?}");
    }

    #[test]
    fn a_later_sweep_decides_only_for_the_woken() {
        // A path 0 – 1 – 2 – 3: node 1 joins 2's community.
        let graph = undirected(4, &vec![(0, 0, 1, 1.0), (0, 1, 2, 1.0), (0, 2, 3, 1.0)]);
        let memory = ComputeMemory::new(u64::MAX);
        let mut state = State::new(&memory, &graph, None).unwrap();
        let mut sweep = Sweep::new(&memory, 4).unwrap();
        let count = sweep.take(&[0, 1, 2, 3]);
        assert_eq!(sweep.batch(count).0, [0, 1, 2, 3], "every node starts woken");
        assert_eq!(sweep.take(&[0, 1, 2, 3]), 0);
        state.community[1] = 2;
        let decision = Decision { node: 1, target: 2, to_target: 1.0, to_own: 0.0 };
        sweep.wake_neighbours(&graph, &state, decision);
        let count = sweep.take(&[0, 1, 2, 3]);
        assert_eq!(sweep.batch(count).0, [0], "2 is in the community 1 joined");
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
