//! CSR build throughput: counting sort, deduplication and transpose of a
//! random edge list of about one million edges (DESIGN.md §5.4).

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use warren_core::graph::GraphCsr;

mod random_graph;

use random_graph::{random_edges, EDGES, ID_SPACE};

fn bench_csr_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("csr_build");
    group.throughput(Throughput::Elements(EDGES));
    group.sample_size(10);
    for (name, weighted) in [("unweighted_with_reverse", false), ("weighted_with_reverse", true)] {
        let edges = match random_edges(weighted) {
            Ok(edges) => edges,
            Err(error) => {
                eprintln!("skipping {name}: {error}");
                continue;
            }
        };
        group.bench_function(name, |b| {
            b.iter_batched(
                || edges.clone(),
                |edges| GraphCsr::build(edges, 1, ID_SPACE, true),
                BatchSize::LargeInput,
            )
        });
    }
    group.finish();
}

criterion_group!(benches, bench_csr_build);
criterion_main!(benches);
