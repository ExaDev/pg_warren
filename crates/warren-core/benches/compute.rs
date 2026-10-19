//! SpMV throughput over a random graph of about one million edges, pulled
//! over the forward and reverse CSRs and pushed without a reverse CSR, on
//! one thread and on every available core (DESIGN.md §6.3, §6.9, §12).

use std::thread::available_parallelism;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use warren_core::compute::{spmv, AnyVector, ComputeMemory, Product, Semiring, Vector, Workers};
use warren_core::graph::GraphCsr;
use warren_core::traversal::Uninterrupted;

mod random_graph;

use random_graph::{random_edges, EDGES, ID_SPACE};

fn bench_spmv(c: &mut Criterion) {
    let memory = ComputeMemory::new(u64::MAX);
    let built =
        |reverse| random_edges(false).and_then(|e| GraphCsr::build(e, 1, ID_SPACE, reverse));
    let (Ok(with_reverse), Ok(forward_only)) = (built(true), built(false)) else {
        eprintln!("skipping compute benchmarks: graph build failed");
        return;
    };
    let vector = |value| Vector::filled(&memory, ID_SPACE, 1, value).map(AnyVector::from);
    let (Ok(x), Ok(mut y)) = (vector(1.0f64), vector(0.0f64)) else {
        eprintln!("skipping compute benchmarks: vector allocation failed");
        return;
    };
    let cores = available_parallelism().map_or(1, |n| n.get());
    let mut group = c.benchmark_group("spmv");
    group.throughput(Throughput::Elements(EDGES));
    group.sample_size(10);
    for threads in [1, cores] {
        let Ok(workers) = Workers::new(threads) else { continue };
        let cases = [
            ("pull_forward", &with_reverse, false),
            ("pull_reverse", &with_reverse, true),
            ("push_forward", &forward_only, true),
        ];
        for (name, graph, transpose) in cases {
            let product = Product { transpose, ..Product::new(Semiring::PlusTimes) };
            group.bench_function(format!("{name}/threads_{threads}"), |b| {
                b.iter(|| spmv(&workers, &memory, &mut Uninterrupted, graph, &product, &x, &mut y))
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_spmv);
criterion_main!(benches);
