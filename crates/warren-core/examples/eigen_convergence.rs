//! Runs one `eigen` to convergence on an undirected graph read from a
//! fixture's `edges.csv` (`src,dst`, ids `1..=nodes`) and prints one JSON
//! line: Lanczos steps, seconds, whether it converged, the eigenvalues. The
//! ARPACK comparison in `bench/eigen.py` runs it (DESIGN.md §6.5, §12).
//!
//! ```text
//! cargo run --release -p warren-core --example eigen_convergence -- \
//!     EDGES_CSV NODES OPERATOR WHICH K TOL THREADS PRECISION
//! ```

use std::error::Error;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::time::Instant;

use warren_core::compute::{ComputeMemory, Workers};
use warren_core::graph::{EdgeList, GraphCsr, RelationDictionary};
use warren_core::interrupts::Uninterrupted;
use warren_core::spectral::{eigen, EigenOptions, GraphMatrix, Precision, Resources};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [path, nodes, operator, which, k, tol, threads, precision] = args.as_slice() else {
        return Err("usage: EDGES_CSV NODES OPERATOR WHICH K TOL THREADS PRECISION".into());
    };
    let nodes: u64 = nodes.parse()?;
    let precision = match precision.as_str() {
        "f32" => Precision::F32,
        "f64" => Precision::F64,
        other => return Err(format!("unknown precision {other}").into()),
    };
    let relation = RelationDictionary::new().intern("edge")?;
    let mut edges = EdgeList::for_id_space(nodes, false);
    for line in BufReader::new(File::open(path)?).lines().skip(1) {
        let line = line?;
        let (src, dst) = line.split_once(',').ok_or("expected src,dst")?;
        let (src, dst) = (src.parse::<u64>()? - 1, dst.parse::<u64>()? - 1);
        edges.push(relation, src, dst, 1.0)?;
        if src != dst {
            edges.push(relation, dst, src, 1.0)?;
        }
    }
    let graph = GraphCsr::build(edges, 1, nodes, false)?;
    let workers = Workers::new(threads.parse()?)?;
    let memory = ComputeMemory::new(u64::MAX);
    let matrix = GraphMatrix { symmetrize: false, ..GraphMatrix::new(operator.parse()?) };
    let options = EigenOptions {
        which: which.parse()?,
        tol: tol.parse()?,
        precision,
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
