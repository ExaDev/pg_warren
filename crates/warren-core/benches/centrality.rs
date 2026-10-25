//! Closeness and harmonic centrality (DESIGN.md §6.7), unweighted, so
//! searched in batches of sources, from 16, 64 and 256 sources on four
//! threads. Under `cargo bench` the graph is larger than the caches: one
//! million nodes and ten million random directed edges. Run as a test
//! (`cargo test --all-targets`, unoptimised), it runs each case once over
//! the shared 100K-node, 1M-edge random graph.

use std::env;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use warren_core::compute::{
    closeness_centrality, harmonic_centrality, ComputeMemory, Sampling, ShortestPaths, Workers,
};
use warren_core::graph::{GraphCsr, RelationSelection, Tombstones};
use warren_core::interrupts::Uninterrupted;

mod random_graph;

use random_graph::{random_edges, random_edges_over, ID_SPACE};

/// Dense ids in the graph under `cargo bench`.
const BENCH_ID_SPACE: u64 = 1_000_000;
/// Edges in the graph under `cargo bench`.
const BENCH_EDGES: u64 = 10_000_000;
/// The compute threads.
const THREADS: usize = 4;

fn bench_centrality(c: &mut Criterion) {
    let benching = env::args().any(|arg| arg == "--bench");
    let (id_space, edges) = match benching {
        true => (BENCH_ID_SPACE, random_edges_over(BENCH_ID_SPACE, BENCH_EDGES, false)),
        false => (ID_SPACE, random_edges(false)),
    };
    let memory = ComputeMemory::new(u64::MAX);
    let graph = edges.and_then(|e| GraphCsr::build(e, 1, id_space, false));
    let (Ok(graph), Ok(workers), Ok(none)) =
        (graph, Workers::new(THREADS), Tombstones::new(id_space))
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
    }
    group.finish();
}

criterion_group!(benches, bench_centrality);
criterion_main!(benches);
