//! The structural kernels (DESIGN.md §6.7) over a random graph of about one
//! million edges: core numbers and strong components over it, the
//! topological order and longest paths over its edges oriented from the
//! smaller dense id to the larger (a DAG), and the minimum spanning forest
//! with integer weights from 1 to 100, unweighted and weighted. `triangles`
//! is the yardstick.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use warren_core::compute::{
    core_number, dag_longest_path, minimum_spanning_forest, strong_components, topological_order,
    triangles, ComputeMemory, Reading, Workers,
};
use warren_core::graph::Tombstones;
use warren_core::graph::{EdgeList, GraphCsr, GraphError, RelationDictionary, RelationSelection};
use warren_core::interrupts::Uninterrupted;

mod random_graph;

use random_graph::{random_edges, EDGES, ID_SPACE};

/// The edges read with weights or not, a pair in several relations once.
fn once(weighted: Option<bool>) -> Reading {
    Reading { weighted, per_relation: false }
}

/// [`random_edges`]' pairs from the smaller dense id to the larger, without
/// self-loops, weighted 1 to 100 by their endpoints.
fn dag_edges() -> Result<EdgeList, GraphError> {
    let relation = RelationDictionary::new().intern("edge")?;
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state =
            state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        state >> 33
    };
    let mut edges = EdgeList::for_id_space(ID_SPACE, true);
    for _ in 0..EDGES {
        let (a, b) = (next() % ID_SPACE, next() % ID_SPACE);
        if a != b {
            edges.push(relation, a.min(b), a.max(b), (1 + (a * 7 + b) % 100) as f64)?;
        }
    }
    Ok(edges)
}

fn bench_structure(c: &mut Criterion) {
    let memory = ComputeMemory::new(u64::MAX);
    let graph = random_edges(false).and_then(|e| GraphCsr::build(e, 1, ID_SPACE, false));
    let dag = dag_edges().and_then(|e| GraphCsr::build(e, 1, ID_SPACE, false));
    let (Ok(graph), Ok(dag), Ok(workers), Ok(none)) =
        (graph, dag, Workers::new(1), Tombstones::new(ID_SPACE))
    else {
        eprintln!("skipping structure benchmarks: graph build failed");
        return;
    };
    let (all, mut quiet) = (RelationSelection::All, Uninterrupted);
    let mut group = c.benchmark_group("structure");
    group.throughput(Throughput::Elements(EDGES));
    group.sample_size(10);
    group.bench_function("triangles", |b| {
        b.iter(|| triangles(&workers, &memory, &mut quiet, &graph, all))
    });
    group.bench_function("core_number", |b| {
        b.iter(|| core_number(&workers, &memory, &mut quiet, &graph, all))
    });
    group.bench_function("strong_components", |b| {
        b.iter(|| strong_components(&workers, &memory, &mut quiet, &graph, all))
    });
    group.bench_function("topological_order", |b| {
        b.iter(|| topological_order(&workers, &memory, &mut quiet, &dag, all, &none))
    });
    group.bench_function("dag_longest_path", |b| {
        b.iter(|| dag_longest_path(&workers, &memory, &mut quiet, &dag, all, once(None), &none))
    });
    for (name, weighted) in [("unweighted", false), ("weighted", true)] {
        group.bench_function(format!("minimum_spanning_forest/{name}"), |b| {
            b.iter(|| {
                minimum_spanning_forest(
                    &workers,
                    &memory,
                    &mut quiet,
                    &dag,
                    all,
                    once(Some(weighted)),
                )
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_structure);
criterion_main!(benches);
