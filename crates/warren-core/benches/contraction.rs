//! Contraction over many relations (DESIGN.md §6.6, §5.3): the quotient of
//! a graph shaped like FB15k-237, 300K unweighted edges on 14.5K nodes,
//! in 1 relation and spread over 237, by a partition into 1,450
//! communities, as Louvain's and Leiden's levels contract their graph.
//! Unweighted, a pair in several relations counts once, in the first of
//! them, unless counted per relation.
//!
//! `contraction/<relations>/<counting>`: `once` (the default) or
//! `per_relation`. One thread.

use criterion::{criterion_group, criterion_main, Criterion};
use warren_core::community::{contract, Contraction, Labels};
use warren_core::compute::{AnyVector, ComputeMemory, Vector, Workers};
use warren_core::graph::{
    BuildMemory, CsrLayout, EdgeList, GraphCsr, GraphError, IdMap, Membership, RelationDictionary,
    Tombstones,
};
use warren_core::interrupts::Uninterrupted;

const ID_SPACE: u64 = 14_500;
const EDGES: u64 = 300_000;
const COMMUNITIES: i64 = 1_450;

/// The edges over `relation_count` relations: the same endpoints for every
/// count, a relation drawn for each from a skewed distribution (as a
/// knowledge graph's relations are), so many pairs are in several.
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
    let mut edges = EdgeList::for_id_space(ID_SPACE, false);
    for _ in 0..EDGES {
        // The square of a uniform draw favours the first relations.
        let draw = next() % 1_000;
        let relation = relations[(draw * draw) as usize * relation_count / 1_000_000];
        // Endpoints from a few hundred hubs and the rest, so pairs repeat.
        let source = if next() % 2 == 0 { next() % 300 } else { next() % ID_SPACE };
        edges.push(relation, source, next() % ID_SPACE, 1.0)?;
    }
    Ok(edges)
}

/// Node `n` in community `n mod COMMUNITIES`.
fn membership(memory: &BuildMemory) -> Option<Membership> {
    let compute = ComputeMemory::new(u64::MAX);
    let values = (0..ID_SPACE as i64).map(|node| node % COMMUNITIES).collect();
    let vector = AnyVector::from(Vector::from_values(&compute, ID_SPACE, 1, values).ok()?);
    let labels = Labels::read(&vector, &Tombstones::new(ID_SPACE).ok()?, memory).ok()?;
    let (id_map, _charge) = IdMap::new().rebuild_within(labels.distinct(memory).ok()?).ok()?;
    labels.membership(&id_map, memory).ok().map(|(membership, _charge)| membership)
}

fn bench_contraction(c: &mut Criterion) {
    let mut group = c.benchmark_group("contraction");
    group.sample_size(10);
    let memory = BuildMemory::unlimited();
    let (Ok(workers), Some(membership)) = (Workers::new(1), membership(&memory)) else {
        eprintln!("skipping contraction benchmarks: no workers or membership");
        return;
    };
    for relation_count in [1, 237] {
        let layout = CsrLayout { build_reverse: false, store_merged: true };
        let graph = match edges(relation_count)
            .and_then(|edges| GraphCsr::build_as(edges, relation_count, ID_SPACE, layout))
        {
            Ok(graph) => graph,
            Err(error) => {
                eprintln!("skipping contraction benchmarks: {error}");
                return;
            }
        };
        let mut names = RelationDictionary::new();
        let relations: Vec<_> = graph
            .relations()
            .iter()
            .enumerate()
            .filter_map(|(r, csr)| Some((names.intern(&format!("r{r}")).ok()?, csr.forward())))
            .collect();
        for per_relation in [false, true] {
            let contraction = Contraction {
                weighted: true,
                per_relation,
                drop_self_loops: false,
                undirected: false,
            };
            let counting = if per_relation { "per_relation" } else { "once" };
            group.bench_function(format!("{relation_count}/{counting}"), |b| {
                b.iter(|| {
                    contract(
                        &workers,
                        &mut Uninterrupted,
                        &relations,
                        &membership,
                        contraction,
                        &memory,
                    )
                })
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_contraction);
criterion_main!(benches);
