//! CSR build throughput: counting sort, deduplication and transpose of a
//! random edge list of about one million edges (DESIGN.md §5.4).

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use warren_core::graph::{EdgeList, GraphCsr, GraphError, RelationDictionary};

const ID_SPACE: u64 = 100_000;
const EDGES: u64 = 1_000_000;

/// A fixed pseudo-random edge list (64-bit LCG), so runs are comparable.
fn random_edges(weighted: bool) -> Result<EdgeList, GraphError> {
    let relation = RelationDictionary::new().intern("edge")?;
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state =
            state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        state >> 33
    };
    let mut edges = EdgeList::new(weighted);
    for _ in 0..EDGES {
        let (source, target) = (next() % ID_SPACE, next() % ID_SPACE);
        edges.push(relation, source, target, 1.0)?;
    }
    Ok(edges)
}

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
