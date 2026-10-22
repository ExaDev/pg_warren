//! The sampled shortest-path centralities (DESIGN.md §6.7) over a random
//! directed graph larger than the caches: one million nodes and ten million
//! unweighted edges, from 16, 64 and 256 sources (betweenness from 16 and
//! 64), on four threads.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use warren_core::compute::{
    betweenness_centrality, closeness_centrality, harmonic_centrality, BetweennessScale,
    ComputeMemory, Sampling, ShortestPaths, Workers,
};
use warren_core::graph::{
    EdgeList, GraphCsr, GraphError, RelationDictionary, RelationSelection, Tombstones,
};
use warren_core::interrupts::Uninterrupted;

/// Dense ids in the graph.
const ID_SPACE: u64 = 1_000_000;
/// Edges in the graph.
const EDGES: u64 = 10_000_000;
/// The compute threads.
const THREADS: usize = 4;

/// [`EDGES`] uniformly random edges over [`ID_SPACE`] (64-bit LCG).
fn random_edges() -> Result<EdgeList, GraphError> {
    let relation = RelationDictionary::new().intern("edge")?;
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state =
            state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        state >> 33
    };
    let mut edges = EdgeList::for_id_space(ID_SPACE, false);
    for _ in 0..EDGES {
        let (source, target) = (next() % ID_SPACE, next() % ID_SPACE);
        edges.push(relation, source, target, 1.0)?;
    }
    Ok(edges)
}

fn bench_centrality(c: &mut Criterion) {
    let memory = ComputeMemory::new(u64::MAX);
    let graph = random_edges().and_then(|e| GraphCsr::build(e, 1, ID_SPACE, false));
    let (Ok(graph), Ok(workers), Ok(none)) =
        (graph, Workers::new(THREADS), Tombstones::new(ID_SPACE))
    else {
        eprintln!("skipping centrality benchmarks: graph build failed");
        return;
    };
    let paths = ShortestPaths {
        graph: &graph,
        relations: RelationSelection::All,
        tombstones: &none,
        directed: true,
        weighted: Some(false),
    };
    let scale = BetweennessScale { normalized: true, endpoints: false };
    let mut quiet = Uninterrupted;
    let mut group = c.benchmark_group("centrality");
    group.sample_size(10);
    for k in [16, 64, 256] {
        let sampling = Sampling { k: Some(k), seed: 1, population: None };
        group.throughput(Throughput::Elements(k));
        group.bench_function(BenchmarkId::new("closeness", k), |b| {
            b.iter(|| closeness_centrality(&workers, &memory, &mut quiet, &paths, &sampling, true))
        });
        group.bench_function(BenchmarkId::new("harmonic", k), |b| {
            b.iter(|| harmonic_centrality(&workers, &memory, &mut quiet, &paths, &sampling))
        });
        if k > 64 {
            // A minute an iteration, one source per thread at a time.
            continue;
        }
        group.bench_function(BenchmarkId::new("betweenness", k), |b| {
            b.iter(|| {
                betweenness_centrality(&workers, &memory, &mut quiet, &paths, &sampling, scale)
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_centrality);
criterion_main!(benches);
