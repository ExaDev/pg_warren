//! A three-step typed walk over a random graph of about one million edges
//! in three relations, in each mode (DESIGN.md §9.2, Appendix A.4).

use criterion::{criterion_group, criterion_main, Criterion};
use warren_core::graph::{EdgeList, GraphCsr, GraphError, RelationDictionary, RelationIndex};
use warren_core::interrupts::Uninterrupted;
use warren_core::traversal::{
    walk, Direction, HopRange, NodeSet, Step, TraversalError, WalkMode, WalkOptions,
};

mod random_graph;

use random_graph::{random_edges, ID_SPACE};

/// The relations of the benchmark graph.
const RELATIONS: [&str; 3] = ["cites", "authored_by", "affiliated_with"];

/// The benchmark edges, dealt round-robin into [`RELATIONS`], and the
/// relations' indexes.
fn typed_graph() -> Result<(GraphCsr, Vec<RelationIndex>), GraphError> {
    let mut dictionary = RelationDictionary::new();
    let relations =
        RELATIONS.iter().map(|name| dictionary.intern(name)).collect::<Result<Vec<_>, _>>()?;
    let mut typed = EdgeList::for_id_space(ID_SPACE, false);
    for (i, (_, source, target)) in random_edges(false)?.edges().enumerate() {
        typed.push(relations[i % relations.len()], source, target, 1.0)?;
    }
    Ok((GraphCsr::build(typed, relations.len(), ID_SPACE, true)?, relations))
}

/// cites 1–3 hops, then authored_by into even nodes, then affiliated_with.
fn steps(relations: &[RelationIndex]) -> Result<Vec<Step>, TraversalError> {
    let step = |relation: RelationIndex, filter, hops| Step {
        relations: Some(vec![relation]),
        direction: Direction::Out,
        filter,
        hops,
    };
    let even = NodeSet::from_dense_ids(ID_SPACE, (0..ID_SPACE).step_by(2))?;
    Ok(vec![
        step(relations[0], None, HopRange::new(1, 3)?),
        step(relations[1], Some(even), HopRange::default()),
        step(relations[2], None, HopRange::default()),
    ])
}

fn bench_walk(c: &mut Criterion) {
    let (graph, relations) = match typed_graph() {
        Ok(typed) => typed,
        Err(error) => return eprintln!("skipping walk benchmarks: {error}"),
    };
    let steps = match steps(&relations) {
        Ok(steps) => steps,
        Err(error) => return eprintln!("skipping walk benchmarks: {error}"),
    };
    let starts: Vec<u64> = (0..100).collect();
    let mut never = Uninterrupted;
    let mut group = c.benchmark_group("walk");
    group.sample_size(10);
    let walks = WalkOptions { distinct_ends: false, ..WalkOptions::default() };
    let capped = WalkOptions { max_results: Some(100_000), ..walks };
    let reverse = WalkOptions { reverse: true, ..WalkOptions::default() };
    for (name, mode, options) in [
        ("ends", WalkMode::Ends, WalkOptions::default()),
        ("ends_reverse", WalkMode::Ends, reverse),
        ("count_walks", WalkMode::Count, walks),
        ("paths_distinct", WalkMode::Paths, WalkOptions::default()),
        ("paths_100k", WalkMode::Paths, capped),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| walk(&graph, &starts, &steps, mode, options, &mut never))
        });
    }
    group.finish();
}

criterion_group!(benches, bench_walk);
criterion_main!(benches);
