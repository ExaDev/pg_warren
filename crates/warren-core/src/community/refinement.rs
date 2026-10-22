//! Leiden's refinement (DESIGN.md §6.7; Traag, Waltman and van Eck, "From
//! Louvain to Leiden: guaranteeing well-connected communities", 2019,
//! Algorithm A.2 `RefinePartition` and `MergeNodesSubset`), for modularity:
//! within each community of a partition, nodes start alone and merge only
//! into refined communities that are well connected to the rest of the
//! community, so every refined community is connected. The next level
//! aggregates by the refined partition but starts from the unrefined one.
//!
//! With `k` a node's degree, `K` a sum of degrees, `2m` the total and `γ`
//! the resolution, in community `S`: a node `v` takes part if it is well
//! connected, `E(v, S − v) ≥ γ k_v (K_S − k_v) / 2m`, and is still alone;
//! it may join a refined community `C ⊆ S` that is well connected,
//! `E(C, S − C) ≥ γ K_C (K_S − K_C) / 2m`, and whose modularity change
//! `ΔH = E(v, C) − γ k_v K_C / 2m` (in units of edge weight, as Traag et
//! al.'s and igraph's: modularity's change times m) is at least 0 (staying
//! alone is `ΔH = 0`); it picks among them at random with probability
//! proportional to `exp(ΔH / θ)`, `θ` the randomness. Nodes are taken in
//! the seeded order, and the draw is a counter-based function of seed and
//! node, so communities refine independently (in parallel) and the result
//! is the same for any thread count.

use std::mem::size_of;

use rayon::prelude::*;

use super::random::{priority, unit};
use super::{CommunityError, Undirected};
use crate::compute::{ComputeError, ComputeMemory, Vector, Workers};
use crate::graph::{DenseId, Tombstones};
use crate::interrupts::{Cancellation, Interrupts, Work};

/// How a refinement runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Refinement {
    /// The resolution `γ`.
    pub resolution: f64,
    /// Orders the nodes and seeds the draws.
    pub seed: u64,
    /// The randomness `θ` of the draws (Traag et al. use 0.01).
    pub randomness: f64,
}

/// A refined partition.
#[derive(Debug)]
pub struct Refined {
    /// Each live node's refined community, labelled by the smallest key of
    /// its members (tombstoned nodes hold 0).
    pub labels: Vector<i64>,
    /// The number of refined communities of live nodes.
    pub communities: u64,
}

/// The stream of [`unit`]'s draws.
const DRAWS: u64 = 1;

/// Refines the partition `communities` labels (one label per node) of the
/// live nodes of `graph`. Refined communities are labelled by the smallest
/// of their members' `keys` (dense ids if `None`).
#[allow(clippy::too_many_arguments)] // a kernel's inputs, as the other kernels take them
pub fn refine<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &Undirected,
    tombstones: &Tombstones,
    communities: &[i64],
    keys: Option<&[i64]>,
    options: Refinement,
) -> Result<Refined, CommunityError> {
    let nodes = graph.id_space();
    for vector in [Some(communities), keys].into_iter().flatten() {
        if vector.len() as u64 != nodes {
            return Err(ComputeError::IdSpaceMismatch {
                expected: nodes,
                found: vector.len() as u64,
            }
            .into());
        }
    }
    let mut labels = Vector::filled(memory, nodes, 1, 0i64)?;
    let longest = (0..nodes).map(|node| graph.row(node).len() as u64).max().unwrap_or(0);
    let threads = workers.threads();
    let scratch = memory.charge(scratch_bytes(nodes, longest, threads))?;
    memory.reclaim_for(Grouping::BYTES_PER_NODE.saturating_mul(nodes.saturating_add(1)));
    let pool = Pool { memory, threads };
    let count = workers.run(interrupts, |cancellation| {
        Ok(run(
            cancellation,
            pool,
            graph,
            tombstones,
            communities,
            keys,
            options,
            labels.values_mut()?,
        ))
    })??;
    drop(scratch);
    Ok(Refined { labels, communities: count })
}

/// What [`refine`] holds on the pool for `nodes` nodes, rows of at most
/// `longest` entries and `threads` threads, charged on the calling thread
/// before it starts: per node its order, place and refined community
/// (24 B); per thread, a row's weights and candidates. What depends on the
/// communities is charged once they are known ([`Grouping`]).
fn scratch_bytes(nodes: u64, longest: u64, threads: usize) -> u64 {
    let per_node = 3 * size_of::<u64>() as u64;
    let per_entry = (size_of::<(usize, f64)>() + size_of::<(usize, f64, f64)>()) as u64;
    nodes
        .saturating_add(1)
        .saturating_mul(per_node)
        .saturating_add((threads as u64).saturating_mul(longest.saturating_add(1) * per_entry))
}

/// The budget [`run`] charges on the pool, and the threads that refine
/// communities at once.
#[derive(Clone, Copy)]
struct Pool<'a> {
    memory: &'a ComputeMemory,
    threads: usize,
}

/// What [`run`] holds once the communities are known, charged on the
/// pool (reclaimed for ahead, at most [`Grouping::BYTES_PER_NODE`] a
/// node): per community its start and its refined communities' list
/// (32 B); the state of the communities being refined, at most one per
/// thread and each at most the largest ([`Group::BYTES_PER_MEMBER`] a
/// member); and the least key of each refined community of one community
/// at a time (8 B a member of the largest).
struct Grouping;

impl Grouping {
    const BYTES_PER_COMMUNITY: u64 = (size_of::<usize>() + size_of::<Vec<usize>>()) as u64;
    const BYTES_PER_LEAST: u64 = size_of::<i64>() as u64;
    /// The most it charges a node: every node a community of its own, or
    /// every node in one community.
    const BYTES_PER_NODE: u64 =
        Self::BYTES_PER_COMMUNITY + Group::BYTES_PER_MEMBER + Self::BYTES_PER_LEAST;

    /// The bytes for `communities` communities of `members` live nodes,
    /// the largest of `largest`, refined by `threads` threads.
    fn bytes(communities: usize, members: usize, largest: usize, threads: usize) -> u64 {
        let refining = members.min(threads.saturating_mul(largest)) as u64;
        (communities as u64)
            .saturating_mul(Self::BYTES_PER_COMMUNITY)
            .saturating_add(refining.saturating_mul(Group::BYTES_PER_MEMBER))
            .saturating_add((largest as u64).saturating_mul(Self::BYTES_PER_LEAST))
    }
}

/// `len` copies of `value`, reserved fallibly; [`refine`] charged them.
fn reserved<T: Clone>(len: usize, value: T) -> Result<Vec<T>, CommunityError> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(len)
        .map_err(|_| ComputeError::OutOfMemory { bytes: (len * size_of::<T>()) as u64 })?;
    buffer.resize(len, value);
    Ok(buffer)
}

/// [`refine`]'s work on the pool: writes the labels and returns the
/// number of refined communities.
#[allow(clippy::too_many_arguments)] // refine's inputs, and the pool's budget
fn run(
    cancellation: &Cancellation,
    pool: Pool<'_>,
    graph: &Undirected,
    tombstones: &Tombstones,
    communities: &[i64],
    keys: Option<&[i64]>,
    options: Refinement,
    labels: &mut [i64],
) -> Result<u64, CommunityError> {
    let nodes = graph.id_space();
    let mut members = reserved(nodes as usize, 0 as DenseId)?;
    members.clear();
    members.extend((0..nodes).filter(|&node| !tombstones.contains(node)));
    members.sort_unstable_by_key(|&node| {
        (communities[node as usize], priority(options.seed, node), node)
    });
    let starts_community =
        |at: usize| communities[members[at - 1] as usize] != communities[members[at] as usize];
    let (mut distinct, mut largest, mut first) = (1, 0, 0);
    for at in (1..members.len()).filter(|&at| starts_community(at)) {
        (distinct, largest, first) = (distinct + 1, largest.max(at - first), at);
    }
    let largest = largest.max(members.len() - first);
    let _grouping =
        pool.memory.charge(Grouping::bytes(distinct, members.len(), largest, pool.threads))?;
    let mut starts = reserved(1, 0usize)?;
    starts
        .try_reserve_exact(distinct + 1)
        .map_err(|_| ComputeError::OutOfMemory { bytes: (distinct * 8) as u64 })?;
    starts.extend((1..members.len()).filter(|&at| starts_community(at)));
    starts.push(members.len());
    let mut place = reserved(nodes as usize, usize::MAX)?;
    for range in starts.windows(2) {
        for (at, &node) in members[range[0]..range[1]].iter().enumerate() {
            place[node as usize] = at;
        }
    }
    let refined: Vec<Vec<usize>> = starts
        .par_windows(2)
        .map(|range| {
            let mut work = Work::new(cancellation);
            work.check()?;
            Group::new(graph, &members[range[0]..range[1]], &place)?.refine(options, &mut work)
        })
        .collect::<Result<_, CommunityError>>()?;
    let key = |node: DenseId| keys.map_or(node as i64, |keys| keys[node as usize]);
    let mut count = 0;
    let mut least = Vec::new();
    for (range, roots) in starts.windows(2).zip(&refined) {
        let members = &members[range[0]..range[1]];
        least.clear();
        least
            .try_reserve_exact(members.len())
            .map_err(|_| ComputeError::OutOfMemory { bytes: (members.len() * 8) as u64 })?;
        least.resize(members.len(), i64::MAX);
        for (at, &root) in roots.iter().enumerate() {
            least[root] = least[root].min(key(members[at]));
        }
        count += roots.iter().enumerate().filter(|&(at, &root)| at == root).count() as u64;
        for (at, &root) in roots.iter().enumerate() {
            labels[members[at] as usize] = least[root];
        }
    }
    Ok(count)
}

/// One community being refined: its members in the seeded order, and each
/// member's refined community (a member standing for it) with that
/// community's degree sum, its weight to the rest of the community and its
/// size.
struct Group<'a> {
    graph: &'a Undirected,
    members: &'a [DenseId],
    place: &'a [usize],
    root: Vec<usize>,
    sums: Vec<f64>,
    outside: Vec<f64>,
    sizes: Vec<usize>,
}

impl<'a> Group<'a> {
    /// The bytes per member its state holds beside its refined
    /// communities (which [`refine`] charges per node, as it returns them).
    const BYTES_PER_MEMBER: u64 = (2 * size_of::<f64>() + size_of::<usize>()) as u64;

    fn new(
        graph: &'a Undirected,
        members: &'a [DenseId],
        place: &'a [usize],
    ) -> Result<Self, CommunityError> {
        let n = members.len();
        let mut root = reserved(n, 0)?;
        root.iter_mut().enumerate().for_each(|(at, root)| *root = at);
        let mut sums = reserved(n, 0.0)?;
        sums.iter_mut().zip(members).for_each(|(sum, &node)| *sum = graph.degree(node));
        Ok(Self {
            graph,
            members,
            place,
            root,
            sums,
            outside: reserved(n, 0.0)?,
            sizes: reserved(n, 1)?,
        })
    }

    /// Whether `node` (a member) is in this group: its place points at it.
    fn holds(&self, node: DenseId) -> Option<usize> {
        let at = self.place[node as usize];
        (self.members.get(at) == Some(&node)).then_some(at)
    }

    /// Each member's refined community, as the place of the member
    /// standing for it.
    fn refine(
        mut self,
        options: Refinement,
        work: &mut Work,
    ) -> Result<Vec<usize>, CommunityError> {
        let total = self.graph.total();
        if total == 0.0 {
            return Ok(self.root);
        }
        let scale = options.resolution / total;
        let group_sum: f64 = self.sums.iter().sum();
        for at in 0..self.members.len() {
            let node = self.members[at];
            work.spend(1 + self.graph.row(node).len() as u64)?;
            self.outside[at] = self
                .graph
                .row(node)
                .iter()
                .filter_map(|&(neighbour, weight)| {
                    self.holds(neighbour).filter(|&other| other != at).map(|_| weight)
                })
                .sum();
        }
        let mut weights: Vec<(usize, f64)> = Vec::new();
        let mut candidates: Vec<(usize, f64, f64)> = Vec::new();
        for at in 0..self.members.len() {
            let node = self.members[at];
            work.spend(1 + self.graph.row(node).len() as u64)?;
            let degree = self.graph.degree(node);
            let alone = self.root[at] == at && self.sizes[at] == 1;
            if !alone || self.outside[at] < scale * degree * (group_sum - degree) {
                continue;
            }
            let row = self.graph.row(node).len();
            weights.clear();
            weights
                .try_reserve(row)
                .map_err(|_| ComputeError::OutOfMemory { bytes: row as u64 * 16 })?;
            for &(neighbour, weight) in self.graph.row(node) {
                if let Some(other) = self.holds(neighbour).filter(|&other| other != at) {
                    weights.push((self.root[other], weight));
                }
            }
            weights.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
            let to_self = self.outside[at];
            candidates.clear();
            candidates
                .try_reserve(row + 1)
                .map_err(|_| ComputeError::OutOfMemory { bytes: (row as u64 + 1) * 24 })?;
            candidates.push((at, 0.0, 0.0));
            let mut i = 0;
            while i < weights.len() {
                let root = weights[i].0;
                let mut to = 0.0;
                while i < weights.len() && weights[i].0 == root {
                    to += weights[i].1;
                    i += 1;
                }
                let sum = self.sums[root];
                let well_connected = self.outside[root] >= scale * sum * (group_sum - sum);
                // ΔH in units of edge weight, as Traag et al.'s θ is.
                let change = to - scale * degree * sum;
                if well_connected && change >= 0.0 {
                    candidates.push((root, to, change));
                }
            }
            let chosen = draw(&candidates, options, node);
            if chosen.0 != at {
                let (root, to, _) = chosen;
                self.root[at] = root;
                self.sizes[root] += 1;
                self.sizes[at] = 0;
                self.sums[root] += degree;
                self.outside[root] += to_self - 2.0 * to;
            }
        }
        Ok(self.root)
    }
}

/// One of `candidates` (staying first), drawn with probability
/// proportional to `exp(change / θ)` by `node`'s counter-based draw. A
/// randomness of 0 takes the greatest change (the first on a tie).
fn draw(candidates: &[(usize, f64, f64)], options: Refinement, node: DenseId) -> (usize, f64, f64) {
    let greatest = candidates.iter().map(|c| c.2).fold(f64::NEG_INFINITY, f64::max);
    if options.randomness <= 0.0 {
        return candidates.iter().copied().find(|c| c.2 == greatest).unwrap_or(candidates[0]);
    }
    let weight = |change: f64| ((change - greatest) / options.randomness).exp();
    let total: f64 = candidates.iter().map(|c| weight(c.2)).sum();
    let mut target = unit(options.seed, DRAWS, node) * total;
    for &candidate in candidates {
        target -= weight(candidate.2);
        if target < 0.0 {
            return candidate;
        }
    }
    candidates[candidates.len() - 1]
}

// Not under Miri: the compute pool's work stealing runs through
// crossbeam-epoch, which Miri's Stacked Borrows model rejects.
#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::OnceLock;

    use proptest::prelude::*;

    use super::*;
    use crate::community::modularity;
    use crate::compute::reference::{self, Edges};
    use crate::interrupts::Uninterrupted;

    fn workers() -> &'static Workers {
        static WORKERS: OnceLock<Workers> = OnceLock::new();
        WORKERS.get_or_init(|| Workers::new(3).unwrap())
    }

    fn undirected(n: u64, edges: &Edges) -> Undirected {
        let mut both = edges.clone();
        both.extend(edges.iter().filter(|e| e.1 != e.2).map(|&(r, s, t, w)| (r, t, s, w)));
        let graph = reference::graph(n, 1, &both, true, false);
        Undirected::new(&ComputeMemory::new(u64::MAX), &Cancellation::new(), &graph, true, false)
            .unwrap()
    }

    const OPTIONS: Refinement = Refinement { resolution: 1.0, seed: 0, randomness: 0.01 };

    fn refined(
        graph: &Undirected,
        communities: &[i64],
        workers: &Workers,
        options: Refinement,
    ) -> Refined {
        let tombstones = Tombstones::new(graph.id_space()).unwrap();
        let memory = ComputeMemory::new(u64::MAX);
        refine(workers, &memory, &mut Uninterrupted, graph, &tombstones, communities, None, options)
            .unwrap()
    }

    /// Whether the nodes of each label are connected by edges among
    /// themselves.
    fn connected(graph: &Undirected, labels: &[i64]) -> bool {
        let mut by_label: BTreeMap<i64, Vec<u64>> = BTreeMap::new();
        for (node, &label) in labels.iter().enumerate() {
            by_label.entry(label).or_default().push(node as u64);
        }
        by_label.values().all(|members| {
            let set: BTreeSet<u64> = members.iter().copied().collect();
            let mut seen = BTreeSet::from([members[0]]);
            let mut todo = vec![members[0]];
            while let Some(node) = todo.pop() {
                for &(neighbour, _) in graph.row(node) {
                    if set.contains(&neighbour) && seen.insert(neighbour) {
                        todo.push(neighbour);
                    }
                }
            }
            seen.len() == set.len()
        })
    }

    #[test]
    fn a_community_of_two_cliques_without_an_edge_between_splits() {
        // One community holding two triangles that no edge joins.
        let edges: Edges = vec![
            (0, 0, 1, 1.0),
            (0, 1, 2, 1.0),
            (0, 0, 2, 1.0),
            (0, 3, 4, 1.0),
            (0, 4, 5, 1.0),
            (0, 3, 5, 1.0),
        ];
        let graph = undirected(6, &edges);
        let found = refined(&graph, &[1; 6], workers(), OPTIONS);
        let labels = found.labels.values();
        assert!(labels[..3].iter().all(|label| !labels[3..].contains(label)), "{labels:?}");
        assert!(connected(&graph, labels));
        assert!(found.communities < 6, "nodes merge: {labels:?}");
    }

    /// Twenty planted blocks of 30 nodes, about 8 edges a node inside its
    /// block and one in ten to another block.
    fn planted() -> Undirected {
        let mix = |x: u64| {
            let z = x.wrapping_mul(0x9e37_79b9_7f4a_7c15);
            (z ^ (z >> 31)).wrapping_mul(0xbf58_476d_1ce4_e5b9) >> 17
        };
        let mut edges = Edges::new();
        for node in 0..600u64 {
            for k in 0..4 {
                let x = mix(node * 8 + k);
                let other = if x % 10 < 9 { node / 30 * 30 + mix(!x) % 30 } else { mix(!x) % 600 };
                edges.push((0, node, other, 1.0));
            }
        }
        undirected(600, &edges)
    }

    #[test]
    fn the_default_randomness_beats_a_randomness_of_one() {
        // θ is in units of edge weight, as in Traag et al.'s and igraph's
        // refinement: at 0.01 the draw nearly always takes the best merge.
        let graph = planted();
        let memory = ComputeMemory::new(u64::MAX);
        let q = |randomness: f64| {
            let found = refined(&graph, &[0; 600], workers(), Refinement { randomness, ..OPTIONS });
            modularity(&memory, &graph, found.labels.values(), 1.0).unwrap()
        };
        let (default, one) = (q(0.01), q(1.0));
        assert!(default > one + 0.01, "θ = 0.01: {default}, θ = 1: {one}");
    }

    #[test]
    fn a_cancel_stops_the_refinement_of_one_large_community() {
        // At resolution 0 a connected graph is one community: the
        // refinement must poll inside it, not only between communities.
        let n = 100_000u64;
        let edges: Edges = (0..n - 1).map(|i| (0, i, i + 1, 1.0)).collect();
        let graph = undirected(n, &edges);
        let members: Vec<DenseId> = (0..n).collect();
        let place: Vec<usize> = (0..n as usize).collect();
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let group = Group::new(&graph, &members, &place).unwrap();
        let refined =
            group.refine(Refinement { resolution: 0.0, ..OPTIONS }, &mut Work::new(&cancellation));
        assert!(matches!(refined, Err(CommunityError::Compute(ComputeError::Cancelled))));
    }

    /// The refinement of a path of `n` nodes in communities of `size`
    /// consecutive nodes, within a budget of `per_node` bytes a node.
    fn refined_within(n: u64, size: u64, per_node: u64) -> Result<Refined, CommunityError> {
        let edges: Edges = (0..n - 1).map(|i| (0, i, i + 1, 1.0)).collect();
        let graph = undirected(n, &edges);
        let communities: Vec<i64> = (0..n).map(|node| (node / size) as i64).collect();
        let tombstones = Tombstones::new(n).unwrap();
        let memory = ComputeMemory::new(n * per_node);
        refine(
            workers(),
            &memory,
            &mut Uninterrupted,
            &graph,
            &tombstones,
            &communities,
            None,
            OPTIONS,
        )
    }

    #[test]
    fn the_state_of_small_communities_is_charged_for_those_being_refined() {
        // The labels (8 B a node), the members, places and refined
        // communities (24 B) and each community's start and list (32 B a
        // community, 3.2 B a node here): the state of a community being
        // refined is held only while a thread refines it.
        let found = refined_within(10_000, 10, 48).unwrap();
        assert_eq!(found.labels.values().len(), 10_000);
    }

    #[test]
    fn the_state_of_one_large_community_is_charged() {
        let refused = refined_within(10_000, 10_000, 48);
        assert!(
            matches!(refused, Err(CommunityError::Compute(ComputeError::MemoryLimit { .. }))),
            "{refused:?}"
        );
    }

    #[test]
    fn the_result_is_the_same_on_any_number_of_threads() {
        let edges: Edges = (0..400u64).map(|i| (0, i % 80, (i * 17 + 1) % 80, 1.0)).collect();
        let graph = undirected(80, &edges);
        let communities: Vec<i64> = (0..80).map(|n| n % 4).collect();
        let one = refined(&graph, &communities, &Workers::new(1).unwrap(), OPTIONS);
        let many = refined(&graph, &communities, workers(), OPTIONS);
        assert_eq!(one.labels.values(), many.labels.values());
    }

    proptest! {
        #[test]
        fn refined_communities_are_connected_and_within_their_community(
            (n, edges, communities, seed) in (2u64..30).prop_flat_map(|n| (
                Just(n),
                prop::collection::vec((0..n, 0..n, 1..4u32), 0..90),
                prop::collection::vec(0i64..4, n as usize),
                any::<u64>(),
            )),
        ) {
            let edges: Edges =
                edges.into_iter().map(|(s, t, w)| (0, s, t, f64::from(w))).collect();
            let graph = undirected(n, &edges);
            let found = refined(&graph, &communities, workers(), Refinement { seed, ..OPTIONS });
            let labels = found.labels.values();
            for u in 0..n as usize {
                for v in 0..n as usize {
                    if labels[u] == labels[v] {
                        prop_assert_eq!(communities[u], communities[v]);
                    }
                }
            }
            prop_assert!(connected(&graph, labels));
        }
    }
}
