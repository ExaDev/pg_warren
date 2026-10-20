//! Times the traversal kernels alone, without SQL: `bfs_each` to a depth
//! and `reachable_set_each` over everything a start reaches, on a directed
//! graph read from a fixture's `edges.csv` (or, given `random:EDGES`, a
//! random graph of `EDGES` edges built in-process), and prints one JSON line per
//! kernel (DESIGN.md §9.3, §12 "Traversal through SQL"). Each figure is
//! the median of `RUNS` runs; the visitor only counts.
//!
//! ```text
//! cargo run --release -p warren-core --example bfs_cost -- EDGES_CSV|random:EDGES NODES START DEPTH RUNS
//! ```

use std::error::Error;
use std::ops::ControlFlow;
use std::time::Instant;

use warren_core::graph::{Direction, RelationSelection};
use warren_core::interrupts::Uninterrupted;
use warren_core::traversal::{bfs_each, reachable_set_each, Adjacency, Limits, TraversalStats};

#[path = "fixture/directed.rs"]
mod directed;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [path, nodes, start, depth, runs] = args.as_slice() else {
        return Err("usage: EDGES_CSV NODES START DEPTH RUNS".into());
    };
    let graph = match path.strip_prefix("random:") {
        Some(edges) => directed::random_graph(nodes.parse()?, edges.parse()?)?,
        None => directed::directed_graph(path, nodes.parse()?)?,
    };
    let adjacency = Adjacency::new(&graph, Direction::Out, RelationSelection::All)?;
    let starts = [start.parse::<u64>()? - 1];
    let limits = Limits { max_depth: Some(depth.parse()?), max_results: None };
    let runs: usize = runs.parse()?;
    report("bfs_each", runs, || {
        let mut rows = 0u64;
        let stats = bfs_each(&adjacency, &starts, None, limits, &mut Uninterrupted, |_| {
            rows += 1;
            ControlFlow::Continue(())
        })?;
        Ok((rows, stats))
    })?;
    report("reachable_set_each", runs, || {
        let mut rows = 0u64;
        let stats = reachable_set_each(&adjacency, &starts, None, &mut Uninterrupted, |_| {
            rows += 1;
            ControlFlow::Continue(())
        })?;
        Ok((rows, stats))
    })
}

/// Runs `kernel` `runs` times and prints the median seconds, the rows it
/// produced and its edges traversed per second.
fn report(
    name: &str,
    runs: usize,
    mut kernel: impl FnMut() -> Result<(u64, TraversalStats), Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    let mut seconds = Vec::with_capacity(runs);
    let mut last = (0, TraversalStats::default());
    for _ in 0..runs.max(1) {
        let started = Instant::now();
        last = kernel()?;
        seconds.push(started.elapsed().as_secs_f64());
    }
    seconds.sort_by(f64::total_cmp);
    let median = seconds[seconds.len() / 2];
    let (rows, stats) = last;
    println!(
        "{{\"kernel\": \"{name}\", \"median_seconds\": {median:e}, \"rows\": {rows}, \"edges_traversed\": {}, \"edges_per_second\": {:e}}}",
        stats.edges_traversed,
        stats.edges_traversed as f64 / median,
    );
    Ok(())
}
