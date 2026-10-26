//! `triangles` (DESIGN.md §6.7) on a skewed graph: an R-MAT graph
//! (Chakrabarti, Zhan and Faloutsos, 2004) of 2¹⁷ dense ids and two
//! million edges, whose degrees follow a power law, as real graphs' do
//! (its largest degree is in the tens of thousands), on one and four
//! threads. The uniform graph of `benches/structure.rs` has no hubs.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use warren_core::compute::{triangles, ComputeMemory, Workers};
use warren_core::graph::{EdgeList, GraphCsr, GraphError, RelationDictionary, RelationSelection};
use warren_core::interrupts::Uninterrupted;

/// Bits of a dense id: the graph has `2^SCALE` dense ids.
const SCALE: u32 = 17;
/// Edges drawn (self-loops and repeats included, as R-MAT draws them).
const EDGES: u64 = 2_000_000;

/// R-MAT edges with Graph500's quadrant probabilities (0.57, 0.19, 0.19,
/// 0.05), from a fixed 64-bit LCG so runs are comparable.
fn rmat_edges() -> Result<EdgeList, GraphError> {
    let relation = RelationDictionary::new().intern("edge")?;
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state =
            state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (state >> 40) % 100
    };
    let mut edges = EdgeList::for_id_space(1 << SCALE, false);
    for _ in 0..EDGES {
        let (mut source, mut target) = (0, 0);
        for _ in 0..SCALE {
            let quadrant = next();
            source = source << 1 | u64::from(quadrant >= 76);
            target = target << 1 | u64::from((57..76).contains(&quadrant) || quadrant >= 95);
        }
        edges.push(relation, source, target, 1.0)?;
    }
    Ok(edges)
}

fn bench_triangles(c: &mut Criterion) {
    let memory = ComputeMemory::new(u64::MAX);
    let Ok(graph) = rmat_edges().and_then(|e| GraphCsr::build(e, 1, 1 << SCALE, false)) else {
        eprintln!("skipping triangles benchmarks: graph build failed");
        return;
    };
    let mut group = c.benchmark_group("triangles/rmat");
    group.throughput(Throughput::Elements(EDGES));
    group.sample_size(10);
    for threads in [1, 4] {
        let Ok(workers) = Workers::new(threads) else {
            eprintln!("skipping triangles benchmarks: no compute pool");
            return;
        };
        group.bench_function(format!("threads/{threads}"), |b| {
            b.iter(|| {
                triangles(&workers, &memory, &mut Uninterrupted, &graph, RelationSelection::All)
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_triangles);
criterion_main!(benches);
