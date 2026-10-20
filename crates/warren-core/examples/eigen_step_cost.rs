//! Splits the cost of one Lanczos step of `eigen` into its SpMV and the
//! rest (orthogonalisation against the basis, restarts and the operator's
//! vector updates), on an undirected graph read from a fixture's
//! `edges.csv`, and prints one JSON line (DESIGN.md §6.5, `bench/eigen.py
//! --step-cost`).
//!
//! A step is timed as `(T(2·STEPS) − T(STEPS)) / STEPS` of `eigen` runs
//! with tolerance 0, so the setup and the final eigenvectors cancel; the
//! SpMV as the mean of `STEPS` plain `spmv` calls over the same CSR.
//!
//! ```text
//! cargo run --release -p warren-core --example eigen_step_cost -- \
//!     EDGES_CSV NODES OPERATOR WHICH K THREADS PRECISION STEPS
//! ```

use std::error::Error;
use std::time::Instant;

use warren_core::compute::{spmv, ComputeMemory, Product, Semiring, Vector, Workers};
use warren_core::graph::GraphCsr;
use warren_core::interrupts::Uninterrupted;
use warren_core::spectral::{eigen, EigenOptions, GraphMatrix, Precision, Resources};

#[path = "fixture/edges.rs"]
mod edges;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [path, nodes, operator, which, k, threads, precision, steps] = args.as_slice() else {
        return Err("usage: EDGES_CSV NODES OPERATOR WHICH K THREADS PRECISION STEPS".into());
    };
    let (nodes, steps): (u64, usize) = (nodes.parse()?, steps.parse()?);
    let precision: Precision = precision.parse()?;
    let graph = edges::undirected_graph(path, nodes)?;
    let workers = Workers::new(threads.parse()?)?;
    let memory = ComputeMemory::new(u64::MAX);
    let matrix = GraphMatrix { symmetrize: false, ..GraphMatrix::new(operator.parse()?) };
    let options = EigenOptions {
        which: which.parse()?,
        tol: 0.0,
        precision,
        ..EigenOptions::new(k.parse()?)
    };
    let solve = |max_iter: usize| -> Result<f64, Box<dyn Error>> {
        let mut resources =
            Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted };
        let start = Instant::now();
        eigen(
            &mut resources,
            &graph,
            &matrix,
            &EigenOptions { max_iter: Some(max_iter), ..options },
        )?;
        Ok(start.elapsed().as_secs_f64())
    };
    let (short, long) = (solve(steps)?, solve(2 * steps)?);
    let step = (long - short) / steps as f64;
    let spmv_seconds = spmv_seconds(&workers, &memory, &graph, steps)?;
    println!(
        "{{\"steps\": {steps}, \"step_seconds\": {step:e}, \"spmv_seconds\": {spmv_seconds:e}, \"rest_seconds\": {:e}, \"nnz\": {}}}",
        step - spmv_seconds,
        graph.all_relations().forward().edge_count(),
    );
    Ok(())
}

/// The mean seconds of one plain `f64` SpMV over every relation.
fn spmv_seconds(
    workers: &Workers,
    memory: &ComputeMemory,
    graph: &GraphCsr,
    steps: usize,
) -> Result<f64, Box<dyn Error>> {
    let rows = graph.all_relations().forward().id_space();
    let x = Vector::filled(memory, rows, 1, 1.0f64)?.into();
    let mut y = Vector::filled(memory, rows, 1, 0.0f64)?.into();
    let product = Product::new(Semiring::PlusTimes);
    let start = Instant::now();
    for _ in 0..steps {
        spmv(workers, memory, &mut Uninterrupted, graph, &product, &x, &mut y)?;
    }
    Ok(start.elapsed().as_secs_f64() / steps as f64)
}
