//! The cost readings over many relations (DESIGN.md §5.3): `dijkstra`,
//! weighted betweenness from 16 sampled sources, `dag_longest_path` and the
//! minimum spanning forest, over the same 100K weighted edges on 20K nodes
//! in 1 relation and spread over 500, with the merged CSR stored and not.
//! A pair in several relations weighs its lightest (for longest paths, its
//! heaviest) relation's weight, so they read every relation's rows: the
//! kernels fold them once, `dijkstra` seeks each relation's row of each
//! node it settles. The edges run from the smaller dense id to the larger,
//! so they are a DAG; a `dijkstra` search settles every node it reaches
//! (its target has no edges).

use criterion::{criterion_group, criterion_main, Criterion};
use warren_core::compute::{
    betweenness_centrality, dag_longest_path, minimum_spanning_forest, BetweennessScale,
    ComputeMemory, Sampling, ShortestPaths, Workers,
};
use warren_core::graph::{
    CsrLayout, Direction, EdgeList, GraphCsr, GraphError, RelationDictionary, RelationSelection,
    Tombstones,
};
use warren_core::interrupts::Uninterrupted;
use warren_core::traversal::{dijkstra, Adjacency};

const ID_SPACE: u64 = 20_000;
const EDGES: u64 = 100_000;

/// The edges over `relation_count` relations: the same endpoints and
/// weights for every count, none into the last node.
fn edges(relation_count: usize) -> Result<EdgeList, GraphError> {
    let mut dictionary = RelationDictionary::new();
    let relations = (0..relation_count)
        .map(|r| dictionary.intern(&format!("r{r}")))
        .collect::<Result<Vec<_>, _>>()?;
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state =
            state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        state >> 33
    };
    let mut edges = EdgeList::for_id_space(ID_SPACE, true);
    for _ in 0..EDGES {
        let relation = relations[next() as usize % relation_count];
        let (a, b) = (next() % (ID_SPACE - 1), next() % (ID_SPACE - 1));
        edges.push(relation, a.min(b), a.max(b), 1.0 + (next() % 100) as f64 / 10.0)?;
    }
    Ok(edges)
}

fn bench_relation_costs(c: &mut Criterion) {
    let mut group = c.benchmark_group("relation_costs");
    group.sample_size(10);
    let (Ok(workers), Ok(none)) = (Workers::new(1), Tombstones::new(ID_SPACE)) else {
        eprintln!("skipping relation cost benchmarks: no workers");
        return;
    };
    let memory = ComputeMemory::new(u64::MAX);
    let all = RelationSelection::All;
    for (relation_count, store_merged) in [(1, true), (500, true), (500, false)] {
        let layout = CsrLayout { build_reverse: false, store_merged };
        let graph = match edges(relation_count)
            .and_then(|edges| GraphCsr::build_as(edges, relation_count, ID_SPACE, layout))
        {
            Ok(graph) => graph,
            Err(error) => {
                eprintln!("skipping relation cost benchmarks: {error}");
                return;
            }
        };
        let Ok(adjacency) = Adjacency::new(&graph, Direction::Out, all) else { continue };
        let merged = if store_merged { "merged" } else { "unmerged" };
        let name = |kernel: &str| format!("{kernel}/{relation_count}_{merged}");
        group.bench_function(name("dijkstra"), |b| {
            b.iter(|| dijkstra(&adjacency, 0, ID_SPACE - 1, &mut Uninterrupted))
        });
        let paths = ShortestPaths {
            graph: &graph,
            relations: all,
            tombstones: &none,
            directed: true,
            weighted: Some(true),
        };
        let sampling = Sampling { k: Some(16), seed: 1, population: None };
        let scale = BetweennessScale { normalized: true, endpoints: false };
        group.bench_function(name("betweenness"), |b| {
            b.iter(|| {
                betweenness_centrality(
                    &workers,
                    &memory,
                    &mut Uninterrupted,
                    &paths,
                    &sampling,
                    scale,
                )
            })
        });
        group.bench_function(name("dag_longest_path"), |b| {
            b.iter(|| {
                dag_longest_path(
                    &workers,
                    &memory,
                    &mut Uninterrupted,
                    &graph,
                    all,
                    Some(true),
                    &none,
                )
            })
        });
        group.bench_function(name("minimum_spanning_forest"), |b| {
            b.iter(|| {
                minimum_spanning_forest(
                    &workers,
                    &memory,
                    &mut Uninterrupted,
                    &graph,
                    all,
                    Some(true),
                )
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_relation_costs);
criterion_main!(benches);
