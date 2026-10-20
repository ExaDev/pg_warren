//! Runs one `eigen` to convergence on an undirected graph read from a
//! fixture's `edges.csv` (`src,dst`, ids `1..=nodes`) and prints one JSON
//! line: Lanczos steps, seconds, whether it converged, the eigenvalues. The
//! ARPACK comparison in `bench/eigen.py` runs it (DESIGN.md §6.5, §12).
//!
//! ```text
//! cargo run --release -p warren-core --example eigen_convergence -- \
//!     EDGES_CSV NODES OPERATOR WHICH K TOL THREADS PRECISION [MAX_ITER]
//! ```
//!
//! `MAX_ITER` is `eigen`'s (default: its own).

use std::error::Error;
use std::time::Instant;

use warren_core::compute::{ComputeMemory, Workers};
use warren_core::interrupts::Uninterrupted;
use warren_core::spectral::{eigen, EigenOptions, GraphMatrix, Resources};

#[path = "fixture/edges.rs"]
mod edges;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (required, max_iter) = match args.len() {
        9 => (&args[..8], Some(args[8].parse()?)),
        _ => (&args[..], None),
    };
    let [path, nodes, operator, which, k, tol, threads, precision] = required else {
        return Err(
            "usage: EDGES_CSV NODES OPERATOR WHICH K TOL THREADS PRECISION [MAX_ITER]".into()
        );
    };
    let graph = edges::undirected_graph(path, nodes.parse()?)?;
    let workers = Workers::new(threads.parse()?)?;
    let memory = ComputeMemory::new(u64::MAX);
    let matrix = GraphMatrix { symmetrize: false, ..GraphMatrix::new(operator.parse()?) };
    let options = EigenOptions {
        which: which.parse()?,
        tol: tol.parse()?,
        precision: precision.parse()?,
        max_iter,
        ..EigenOptions::new(k.parse()?)
    };
    let start = Instant::now();
    let mut resources =
        Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted };
    let result = eigen(&mut resources, &graph, &matrix, &options)?;
    let seconds = start.elapsed().as_secs_f64();
    let values: Vec<String> = result.pairs.iter().map(|p| format!("{:e}", p.value)).collect();
    let residual = result.pairs.iter().map(|p| p.residual).fold(0.0f64, f64::max);
    println!(
        "{{\"steps\": {}, \"seconds\": {seconds:.3}, \"converged\": {}, \"max_residual\": {residual:e}, \"eigenvalues\": [{}]}}",
        result.iterations,
        result.converged,
        values.join(", ")
    );
    Ok(())
}
