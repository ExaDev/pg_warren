//! `dijkstra` over many relations (DESIGN.md §5.3, §9.3): the same 100K
//! weighted edges on 20K nodes in 1 relation and spread over 500, with the
//! merged CSR stored and not, read once (a pair's weights summed) and per
//! relation. A search settles every node it reaches (its target has no
//! edges). Read once from the stored merged CSR, its cost follows the
//! edges it reads, not the number of relations; reading every relation's
//! row (per relation, or once without the merged CSR) costs a row lookup
//! per relation per settled node.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use warren_core::graph::RelationSelection;
use warren_core::graph::{
    CsrLayout, Direction, EdgeList, GraphCsr, GraphError, RelationDictionary,
};
use warren_core::interrupts::Uninterrupted;
use warren_core::traversal::{dijkstra, Adjacency};

const ID_SPACE: u64 = 20_000;
const EDGES: u64 = 100_000;

/// The edges over `relation_count` relations: the same endpoints and
/// weights for every count, none into the last node.
fn edges(relation_count: usize) -> Result<EdgeList, GraphError> {
    let mut dictionary = RelationDictionary::new();
    let relations = (0..relation_count)
        .map(|r| dictionary.intern(&format!("r{r}")))
        .collect::<Result<Vec<_>, _>>()?;
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state =
            state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        state >> 33
    };
    let mut edges = EdgeList::for_id_space(ID_SPACE, true);
    for _ in 0..EDGES {
        let relation = relations[next() as usize % relation_count];
        let (source, target) = (next() % (ID_SPACE - 1), next() % (ID_SPACE - 1));
        edges.push(relation, source, target, 1.0 + (next() % 100) as f64 / 10.0)?;
    }
    Ok(edges)
}

fn bench_dijkstra(c: &mut Criterion) {
    let mut group = c.benchmark_group("dijkstra");
    group.throughput(Throughput::Elements(EDGES));
    group.sample_size(10);
    for (relation_count, store_merged) in [(1, true), (500, true), (500, false)] {
        let layout = CsrLayout { build_reverse: false, store_merged };
        let graph = match edges(relation_count)
            .and_then(|edges| GraphCsr::build_as(edges, relation_count, ID_SPACE, layout))
        {
            Ok(graph) => graph,
            Err(error) => {
                eprintln!("skipping dijkstra benchmarks: {error}");
                return;
            }
        };
        let Ok(adjacency) = Adjacency::new(&graph, Direction::Out, RelationSelection::All) else {
            continue;
        };
        for (reading, per_relation) in [("once", false), ("per_relation", true)] {
            let merged = if store_merged { "merged" } else { "unmerged" };
            group.bench_function(format!("{relation_count}_relations_{merged}_{reading}"), |b| {
                b.iter(|| dijkstra(&adjacency, 0, ID_SPACE - 1, per_relation, &mut Uninterrupted))
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_dijkstra);
criterion_main!(benches);
