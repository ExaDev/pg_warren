//! Lanczos eigenpairs, k = 20, of a random graph of 100K nodes and about
//! one million edges (DESIGN.md §6.5, §12, §14 item 8).
//!
//! Each sample runs a fixed number of Lanczos steps (tolerance 0), so the
//! work per sample is the same and the time per step can be compared
//! across operators, precisions and thread counts. Before sampling, one run
//! per operator to `tol = 1e-6` reports how many steps convergence takes.
//! Run as a test (`cargo test --all-targets`, unoptimised), it only checks
//! that the first case runs, for `k` steps.

use std::env;
use std::thread::available_parallelism;
use std::time::Instant;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use warren_core::compute::{ComputeMemory, Workers};
use warren_core::graph::GraphCsr;
use warren_core::interrupts::Uninterrupted;
use warren_core::spectral::{
    eigen, Eigen, EigenOptions, GraphMatrix, Operator, Precision, Resources, SpectralError, Which,
};

mod random_graph;

use random_graph::{random_edges, ID_SPACE};

/// The Lanczos steps of one sample.
const STEPS: usize = 200;

const CASES: [(&str, Operator, Which); 2] = [
    ("adjacency_largest", Operator::Adjacency, Which::Largest),
    ("normalized_laplacian_smallest", Operator::NormalizedLaplacian, Which::Smallest),
];

fn solve(
    workers: &Workers,
    graph: &GraphCsr,
    operator: Operator,
    options: &EigenOptions,
) -> Result<Eigen, SpectralError> {
    let memory = ComputeMemory::new(u64::MAX);
    let mut resources = Resources { workers, memory: &memory, interrupts: &mut Uninterrupted };
    eigen(&mut resources, graph, &GraphMatrix::new(operator), options)
}

fn bench_eigen(c: &mut Criterion) {
    let Ok(graph) = random_edges(false).and_then(|e| GraphCsr::build(e, 1, ID_SPACE, true)) else {
        eprintln!("skipping spectral benchmarks: graph build failed");
        return;
    };
    let cores = available_parallelism().map_or(1, |n| n.get());
    let Ok(all) = Workers::new(cores) else { return };
    let benching = env::args().any(|arg| arg == "--bench");
    let steps = if benching { STEPS } else { 20 };
    for (name, operator, which) in CASES.into_iter().filter(|_| benching) {
        let options = EigenOptions { which, tol: 1e-6, ..EigenOptions::new(20) };
        let start = Instant::now();
        if let Ok(result) = solve(&all, &graph, operator, &options) {
            let (steps, converged) = (result.iterations, result.converged);
            let seconds = start.elapsed().as_secs_f64();
            eprintln!(
                "{name}/f32/threads_{cores}: tol 1e-6 in {steps} steps, {seconds:.1} s, converged {converged}"
            );
        }
    }
    let mut group = c.benchmark_group("eigen_k20");
    group.sample_size(10);
    group.throughput(Throughput::Elements(steps as u64));
    let mut cases = Vec::new();
    for (name, operator, which) in CASES {
        cases.push((name, operator, which, Precision::F32, "f32", cores));
        cases.push((name, operator, which, Precision::F64, "f64", cores));
    }
    cases.push((
        "adjacency_largest",
        Operator::Adjacency,
        Which::Largest,
        Precision::F32,
        "f32",
        1,
    ));
    cases.truncate(if benching { cases.len() } else { 1 });
    for (name, operator, which, precision, label, threads) in cases {
        let Ok(workers) = Workers::new(threads) else { continue };
        let options = EigenOptions {
            which,
            precision,
            tol: 0.0,
            max_iter: Some(steps),
            ..EigenOptions::new(20)
        };
        group.bench_function(format!("{name}/{label}/threads_{threads}"), |b| {
            b.iter(|| solve(&workers, &graph, operator, &options))
        });
    }
    group.finish();
}

criterion_group!(benches, bench_eigen);
criterion_main!(benches);
