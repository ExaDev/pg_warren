//! Traversal throughput over a random graph of about one million edges:
//! BFS from one node and weakly connected components (DESIGN.md §9.3, §12).

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use warren_core::graph::GraphCsr;
use warren_core::traversal::{
    bfs, components, Adjacency, Direction, Limits, RelationSelection, Uninterrupted,
};

mod random_graph;

use random_graph::{random_edges, EDGES, ID_SPACE};

fn bench_traversal(c: &mut Criterion) {
    let graph =
        match random_edges(false).and_then(|edges| GraphCsr::build(edges, 1, ID_SPACE, true)) {
            Ok(graph) => graph,
            Err(error) => {
                eprintln!("skipping traversal benchmarks: {error}");
                return;
            }
        };
    let mut never = Uninterrupted;
    let mut group = c.benchmark_group("traversal");
    group.throughput(Throughput::Elements(EDGES));
    group.sample_size(10);
    for (name, direction) in [("bfs_out", Direction::Out), ("bfs_both", Direction::Both)] {
        let Ok(adjacency) = Adjacency::new(&graph, direction, RelationSelection::All) else {
            continue;
        };
        group.bench_function(name, |b| {
            b.iter(|| bfs(&adjacency, &[0], None, Limits::default(), &mut never))
        });
    }
    group.bench_function("components", |b| b.iter(|| components(&graph, &mut never)));
    group.finish();
}

criterion_group!(benches, bench_traversal);
criterion_main!(benches);
