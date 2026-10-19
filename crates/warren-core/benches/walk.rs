//! A three-step typed walk over a random graph of about one million edges
//! in three relations, in each mode (DESIGN.md §9.2, Appendix A.4).

use criterion::{criterion_group, criterion_main, Criterion};
use warren_core::graph::{EdgeList, GraphCsr, GraphError, RelationDictionary};
use warren_core::traversal::{
    walk, Direction, HopRange, NodeSet, Step, TraversalError, Uninterrupted, WalkMode, WalkOptions,
};

mod random_graph;

use random_graph::{random_edges, ID_SPACE};

/// The benchmark edges, dealt round-robin into three relations.
fn typed_graph() -> Result<GraphCsr, GraphError> {
    let mut dictionary = RelationDictionary::new();
    let relations = [dictionary.intern("cites")?, dictionary.intern("authored_by")?, {
        dictionary.intern("affiliated_with")?
    }];
    let edges = random_edges(false)?;
    let mut typed = EdgeList::for_id_space(ID_SPACE, false);
    for (i, (_, source, target)) in edges.edges().enumerate() {
        typed.push(relations[i % relations.len()], source, target, 1.0)?;
    }
    GraphCsr::build(typed, relations.len(), ID_SPACE, true)
}

/// cites 1–3 hops, then authored_by into even nodes, then affiliated_with.
fn steps() -> Result<Vec<Step>, TraversalError> {
    let mut dictionary = RelationDictionary::new();
    let mut relation = |name: &str| dictionary.intern(name).map(|r| Some(vec![r]));
    let even = NodeSet::from_dense_ids(ID_SPACE, (0..ID_SPACE).step_by(2))?;
    Ok(vec![
        Step {
            relations: relation("cites")?,
            direction: Direction::Out,
            filter: None,
            hops: HopRange::new(1, 3)?,
        },
        Step {
            relations: relation("authored_by")?,
            direction: Direction::Out,
            filter: Some(even),
            hops: HopRange::default(),
        },
        Step {
            relations: relation("affiliated_with")?,
            direction: Direction::Out,
            filter: None,
            hops: HopRange::default(),
        },
    ])
}

fn bench_walk(c: &mut Criterion) {
    let (graph, steps) = match (typed_graph(), steps()) {
        (Ok(graph), Ok(steps)) => (graph, steps),
        (Err(error), _) => return eprintln!("skipping walk benchmarks: {error}"),
        (_, Err(error)) => return eprintln!("skipping walk benchmarks: {error}"),
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
