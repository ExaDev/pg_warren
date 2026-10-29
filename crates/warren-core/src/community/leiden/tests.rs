//! Leiden against the library's way of running it through 0.7.0
//! (`leiden.sql`), which built every level as a graph: the graph
//! contracted by the composed refined partition with `contract`'s kernel,
//! each level's partition lifted to the graph and the next level's start
//! projected onto it (`min`).

use std::sync::OnceLock;

use proptest::prelude::*;

use super::*;
use crate::community::{contract, lift, project, Aggregate, Contraction, Weighting};
use crate::compute::reference::{self, Edges};
use crate::compute::{AnyVector, ComputeError};
use crate::graph::{BuildMemory, GraphCsr};
use crate::interrupts::{Cancellation, Cancelled, Uninterrupted};

fn workers() -> &'static Workers {
    static WORKERS: OnceLock<Workers> = OnceLock::new();
    WORKERS.get_or_init(|| Workers::new(3).unwrap())
}

const OPTIONS: Leiden = Leiden {
    resolution: 1.0,
    seed: 0,
    max_levels: 100,
    tolerance: 1e-7,
    randomness: 0.01,
    iterations: Some(2),
};

/// A graph to run on: its CSRs, deleted nodes, and how its view reads it.
struct Graph {
    csr: GraphCsr,
    relations: usize,
    tombstones: Tombstones,
    weighting: Weighting,
    directed: bool,
}

impl Graph {
    /// `edges` among `n` nodes in `relations` relations, stored both ways
    /// unless `directed`; `deleted` nodes (which must have no edges) are
    /// tombstoned.
    fn new(n: u64, relations: usize, edges: &Edges, deleted: &[u64], directed: bool) -> Self {
        let mut stored = edges.clone();
        if !directed {
            stored.extend(edges.iter().filter(|e| e.1 != e.2).map(|&(r, s, t, w)| (r, t, s, w)));
        }
        let mut tombstones = Tombstones::new(n).unwrap();
        deleted.iter().for_each(|&node| tombstones.insert(node).unwrap());
        Self {
            csr: reference::graph(n, relations, &stored, true, false),
            relations,
            tombstones,
            weighting: Weighting { weighted: true, per_relation: false },
            directed,
        }
    }

    fn view(&self, memory: &ComputeMemory) -> Result<Undirected, CommunityError> {
        let cancellation = Cancellation::new();
        Undirected::new(memory, &cancellation, &self.csr, self.weighting, self.directed)
    }
}

/// A port over a [`Graph`] that keeps what it is told.
struct Recording<'a> {
    graph: &'a Graph,
    memory: &'a ComputeMemory,
    keeps: bool,
    levels: Vec<(u32, u32)>,
    kept: Vec<(u32, Vec<i64>)>,
    views: u32,
}

impl<'a> Recording<'a> {
    fn new(graph: &'a Graph, memory: &'a ComputeMemory, keeps: bool) -> Self {
        Self { graph, memory, keeps, levels: Vec::new(), kept: Vec::new(), views: 0 }
    }
}

impl Multilevel for Recording<'_> {
    type Error = CommunityError;
    fn view(&mut self) -> Result<Undirected, CommunityError> {
        self.views += 1;
        self.graph.view(self.memory)
    }
    fn level(&mut self, iteration: u32, level: u32) -> Result<(), CommunityError> {
        self.levels.push((iteration, level));
        Ok(())
    }
    fn keeps_levels(&self) -> bool {
        self.keeps
    }
    fn keep_level(&mut self, ordinal: u32, labels: Vector<i64>) -> Result<(), CommunityError> {
        self.kept.push((ordinal, labels.values().to_vec()));
        Ok(())
    }
}

/// What a run found, as the reference and the kernel both report it.
#[derive(Debug, PartialEq)]
struct Outcome {
    labels: Vec<i64>,
    modularity: u64,
    stable: bool,
    levels: Vec<(u32, u32)>,
    kept: Vec<(u32, Vec<i64>)>,
}

fn run(graph: &Graph, options: Leiden) -> Outcome {
    let memory = ComputeMemory::new(u64::MAX);
    let mut port = Recording::new(graph, &memory, true);
    let found =
        leiden(workers(), &memory, &mut Uninterrupted, &graph.tombstones, options, &mut port)
            .unwrap();
    Outcome {
        labels: found.labels.values().to_vec(),
        modularity: found.modularity.to_bits(),
        stable: found.stable,
        levels: port.levels,
        kept: port.kept,
    }
}

/// A partition of the graph's live nodes as the membership of a quotient
/// whose nodes are its labels in order, and those labels.
fn membership_of(labels: &[i64], tombstones: &Tombstones) -> (Membership, Vec<i64>) {
    let live = |node: usize| !tombstones.contains(node as u64);
    let mut keys: Vec<i64> = (0..labels.len()).filter(|&n| live(n)).map(|n| labels[n]).collect();
    keys.sort_unstable();
    keys.dedup();
    let stored = (0..labels.len())
        .map(|n| match live(n) {
            true => keys.binary_search(&labels[n]).unwrap() as u64,
            false => u64::MAX,
        })
        .collect();
    (Membership::from_stored(stored, keys.len() as u64).unwrap(), keys)
}

fn integers(vector: AnyVector) -> Vec<i64> {
    match vector {
        AnyVector::I64(vector) => vector.values().to_vec(),
        AnyVector::F64(vector) => vector.values().iter().map(|&v| v as i64).collect(),
        _ => unreachable!(),
    }
}

/// One level as the library built it: a graph contracted from the
/// graph, its view and its id space's (empty) tombstones.
struct LibraryLevel {
    view: Undirected,
    tombstones: Tombstones,
    keys: Vec<i64>,
    membership: Membership,
    initial: Vec<i64>,
}

/// `leiden.sql` over the primitives, step for step.
fn library(graph: &Graph, options: Leiden) -> Outcome {
    let memory = ComputeMemory::new(u64::MAX);
    let first = graph.view(&memory).unwrap();
    let nodes_of_graph = graph.csr.all_relations().forward().id_space();
    let live = nodes_of_graph - graph.tombstones.count();
    let mut levels = Vec::new();
    let mut kept = Vec::new();
    let mut previous: Option<Vec<i64>> = None;
    let mut iteration = 0;
    loop {
        iteration += 1;
        let mut level = 0;
        let mut at: Option<LibraryLevel> = None;
        let down = loop {
            level += 1;
            levels.push((iteration, level));
            let (view, tombstones, nodes) = match &at {
                None => (&first, &graph.tombstones, live),
                Some(at) => (&at.view, &at.tombstones, at.view.id_space()),
            };
            let keys = at.as_ref().map(|at| &at.keys[..]);
            let initial = match &at {
                None => previous.as_deref(),
                Some(at) => Some(&at.initial[..]),
            };
            let moving = LocalMoving {
                resolution: options.resolution,
                seed: options.seed,
                max_sweeps: 100,
                tolerance: options.tolerance,
            };
            let moved = local_moving(
                workers(),
                &memory,
                &mut Uninterrupted,
                view,
                tombstones,
                initial,
                keys,
                moving,
            )
            .unwrap();
            let lifted = |labels: &Vector<i64>| match &at {
                None => labels.values().to_vec(),
                Some(at) => {
                    integers(lift(&memory, &at.membership, &AnyVector::I64(copy(labels))).unwrap())
                }
            };
            let mut down = lifted(&moved.labels);
            let mut refined_down = None;
            let mut refined_count = None;
            if moved.communities < nodes {
                let refinement = Refinement {
                    resolution: options.resolution,
                    seed: options.seed,
                    randomness: options.randomness,
                };
                let refined = refine(
                    workers(),
                    &memory,
                    &mut Uninterrupted,
                    view,
                    tombstones,
                    moved.labels.values(),
                    keys,
                    refinement,
                )
                .unwrap();
                refined_count = Some(refined.communities);
                let lifted_refined = lifted(&refined.labels);
                if level >= options.max_levels || refined.communities == nodes {
                    down = lifted_refined.clone();
                }
                refined_down = Some(lifted_refined);
            }
            kept.push((kept.len() as u32 + 1, down.clone()));
            if moved.communities == nodes
                || level >= options.max_levels
                || refined_count == Some(nodes)
            {
                break down;
            }
            let refined_down = refined_down.unwrap();
            at = Some(contracted(graph, &down, &refined_down, &memory));
        };
        if options.iterations == Some(iteration) {
            return outcome(&first, &memory, options, down, true, levels, kept);
        }
        if previous.as_ref() == Some(&down) {
            return outcome(&first, &memory, options, down, true, levels, kept);
        }
        if options.iterations.is_none() && iteration == MOST_ITERATIONS {
            return outcome(&first, &memory, options, down, false, levels, kept);
        }
        previous = Some(down);
    }
}

/// The level after one: the graph contracted by `refined_down` (by
/// `contract`'s kernel, built as a graph and viewed), starting from
/// `down` projected onto it with `min`.
fn contracted(
    graph: &Graph,
    down: &[i64],
    refined_down: &[i64],
    memory: &ComputeMemory,
) -> LibraryLevel {
    let (membership, keys) = membership_of(refined_down, &graph.tombstones);
    let indexes = reference::relations(graph.relations);
    let relations: Vec<_> =
        indexes.iter().zip(graph.csr.relations()).map(|(&r, csr)| (r, csr.forward())).collect();
    let contraction = Contraction {
        weighted: graph.weighting.weighted,
        per_relation: graph.weighting.per_relation,
        drop_self_loops: false,
        undirected: !graph.directed,
    };
    let edges = contract(
        workers(),
        &mut Uninterrupted,
        &relations,
        &membership,
        contraction,
        &BuildMemory::unlimited(),
    )
    .unwrap();
    let quotient: Edges = edges
        .edges()
        .zip(edges.weights().unwrap())
        .map(|((r, s, t), &w)| (r.index(), s, t, w))
        .collect();
    let k = keys.len() as u64;
    let csr = reference::graph(k, graph.relations, &quotient, true, false);
    let weighting = Weighting { weighted: true, per_relation: false };
    let cancellation = Cancellation::new();
    let view = Undirected::new(memory, &cancellation, &csr, weighting, graph.directed).unwrap();
    let down = Vector::from_values(memory, down.len() as u64, 1, down.to_vec()).unwrap();
    let initial =
        integers(project(memory, &membership, &AnyVector::I64(down), Aggregate::Min).unwrap());
    LibraryLevel { view, tombstones: Tombstones::new(k).unwrap(), keys, membership, initial }
}

fn copy(labels: &Vector<i64>) -> Vector<i64> {
    let memory = ComputeMemory::new(u64::MAX);
    Vector::from_values(&memory, labels.id_space(), 1, labels.values().to_vec()).unwrap()
}

fn outcome(
    first: &Undirected,
    memory: &ComputeMemory,
    options: Leiden,
    labels: Vec<i64>,
    stable: bool,
    levels: Vec<(u32, u32)>,
    kept: Vec<(u32, Vec<i64>)>,
) -> Outcome {
    let q = modularity(memory, first, &labels, options.resolution).unwrap();
    Outcome { labels, modularity: q.to_bits(), stable, levels, kept }
}

/// A ring of `cliques` 5-cliques, each joined to the next by one edge.
fn ring_of_cliques(cliques: u64) -> Edges {
    let mut edges = Edges::new();
    for c in 0..cliques {
        for a in 0..5 {
            for b in a + 1..5 {
                edges.push((0, 5 * c + a, 5 * c + b, 1.0));
            }
        }
        edges.push((0, 5 * c + 4, (5 * c + 5) % (5 * cliques), 1.0));
    }
    edges
}

#[test]
fn it_finds_a_ring_of_cliques() {
    let graph = Graph::new(30, 1, &ring_of_cliques(6), &[], false);
    let found = run(&graph, OPTIONS);
    let cliques: Vec<i64> = (0..30).map(|n| n / 5 * 5).collect();
    assert_eq!(found.labels, cliques);
    assert_eq!(found, library(&graph, OPTIONS));
}

#[test]
fn a_deleted_node_is_in_no_community() {
    let edges: Edges = ring_of_cliques(6).into_iter().filter(|e| e.1 != 2 && e.2 != 2).collect();
    let graph = Graph::new(30, 1, &edges, &[2], false);
    let found = run(&graph, OPTIONS);
    assert_eq!(found.labels[2], 0);
    assert_eq!(found.labels[..5], [0, 0, 0, 0, 0]);
    assert_eq!(found, library(&graph, OPTIONS));
}

#[test]
fn a_graph_without_edges_keeps_every_node_alone() {
    let graph = Graph::new(3, 1, &Edges::new(), &[], true);
    let found = run(&graph, OPTIONS);
    assert_eq!(found.labels, [0, 1, 2]);
    assert_eq!(f64::from_bits(found.modularity), 0.0);
    assert_eq!(found.levels, [(1, 1), (2, 1)], "one level each, and the second changed nothing");
    assert_eq!(found, library(&graph, OPTIONS));
}

#[test]
fn it_keeps_the_graphs_view_while_it_runs_on_the_first_level() {
    // Every iteration on a graph without edges ends at its first level, so
    // the view is built once.
    let graph = Graph::new(3, 1, &Edges::new(), &[], true);
    let memory = ComputeMemory::new(u64::MAX);
    let mut port = Recording::new(&graph, &memory, false);
    let options = Leiden { iterations: None, ..OPTIONS };
    leiden(workers(), &memory, &mut Uninterrupted, &graph.tombstones, options, &mut port).unwrap();
    assert_eq!(port.views, 1);
    assert!(port.kept.is_empty(), "it keeps no level unless asked");
}

#[test]
fn it_holds_nothing_once_it_returns_but_its_partition() {
    let graph = Graph::new(30, 1, &ring_of_cliques(6), &[], false);
    let memory = ComputeMemory::new(u64::MAX);
    let mut port = Recording::new(&graph, &memory, true);
    let found =
        leiden(workers(), &memory, &mut Uninterrupted, &graph.tombstones, OPTIONS, &mut port)
            .unwrap();
    assert_eq!(memory.in_use(), found.labels.bytes());
}

#[test]
fn it_stops_within_its_budget() {
    let graph = Graph::new(30, 1, &ring_of_cliques(6), &[], false);
    let memory = ComputeMemory::new(2048);
    let mut port = Recording::new(&graph, &memory, false);
    let refused =
        leiden(workers(), &memory, &mut Uninterrupted, &graph.tombstones, OPTIONS, &mut port);
    assert!(
        matches!(refused, Err(CommunityError::Compute(ComputeError::MemoryLimit { .. }))),
        "{refused:?}"
    );
}

#[test]
fn a_cancel_stops_it() {
    struct Cancel;
    impl Interrupts for Cancel {
        fn check(&mut self) -> Result<(), Cancelled> {
            Err(Cancelled)
        }
    }
    let graph = Graph::new(30, 1, &ring_of_cliques(6), &[], false);
    let memory = ComputeMemory::new(u64::MAX);
    let mut port = Recording::new(&graph, &memory, false);
    let stopped = leiden(workers(), &memory, &mut Cancel, &graph.tombstones, OPTIONS, &mut port);
    assert!(matches!(stopped, Err(CommunityError::Compute(ComputeError::Cancelled))));
}

#[test]
fn a_refused_randomness_is_the_refinements() {
    let graph = Graph::new(30, 1, &ring_of_cliques(6), &[], false);
    let refused = {
        let memory = ComputeMemory::new(u64::MAX);
        let mut port = Recording::new(&graph, &memory, false);
        let options = Leiden { randomness: -1.0, ..OPTIONS };
        leiden(workers(), &memory, &mut Uninterrupted, &graph.tombstones, options, &mut port)
            .map(|found| found.modularity)
    };
    assert!(matches!(refused, Err(CommunityError::Randomness(r)) if r == -1.0));
}

/// Random graphs of planted blocks: `n` nodes in blocks of 6, most edges
/// inside a block, some nodes deleted, in up to three relations.
fn blocks() -> impl Strategy<Value = (u64, usize, Edges, Vec<u64>)> {
    (6u64..60, 1usize..4).prop_flat_map(|(n, relations)| {
        (
            Just(n),
            Just(relations),
            prop::collection::vec(
                (0..relations, 0..n, 0u64..6, any::<bool>(), 1..4u32),
                n as usize..3 * n as usize,
            ),
            prop::collection::vec(0..n, 0..3),
        )
            .prop_map(|(n, relations, picks, deleted)| {
                let edges = picks
                    .into_iter()
                    .map(|(r, s, offset, across, w)| {
                        let t = match across {
                            true => (s * 7 + offset * 13) % n,
                            false => (s / 6 * 6 + offset).min(n - 1),
                        };
                        (r, s, t, f64::from(w))
                    })
                    .filter(|&(_, s, t, _)| !deleted.contains(&s) && !deleted.contains(&t))
                    .collect();
                (n, relations, edges, deleted)
            })
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Levels held in memory, each contracted from the one before, give
    /// what the library's levels built as graphs from the graph gave: the
    /// same partition, levels, kept partitions and modularity, to the bit.
    #[test]
    fn it_finds_what_the_library_found(
        (n, relations, edges, deleted) in blocks(),
        directed in any::<bool>(),
        weighted in any::<bool>(),
        per_relation in any::<bool>(),
        seed in 0u64..4,
        resolution in prop::sample::select(vec![0.5, 1.0, 2.0]),
        randomness in prop::sample::select(vec![0.0, 0.01, 1.0]),
        max_levels in 1u32..5,
        iterations in prop::sample::select(vec![Some(1), Some(2), Some(3), None]),
    ) {
        let mut graph = Graph::new(n, relations, &edges, &deleted, directed);
        graph.weighting = Weighting { weighted, per_relation };
        let options = Leiden { resolution, seed, max_levels, randomness, iterations, ..OPTIONS };
        prop_assert_eq!(run(&graph, options), library(&graph, options));
    }
}
