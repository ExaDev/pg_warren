//! `warren.diff` throughput (DESIGN.md §7, §12): two versions of a graph of
//! 100K nodes and about one million edges in five relations, the later one
//! with 1% of nodes removed, 1% added and about 5% of edges replaced.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use warren_core::graph::{EdgeIntake, GraphCsr, GraphError, IdMap, RelationDictionary};
use warren_core::version::{diff, GraphVersion, VersionError};

const NODES: i64 = 100_000;
const EDGES: u64 = 1_000_000;
const RELATIONS: u64 = 5;

/// A fixed pseudo-random sequence (64-bit LCG), so runs are comparable.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 =
            self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

/// One built version.
struct Version {
    id_map: IdMap<i64>,
    relations: RelationDictionary,
    graph: GraphCsr,
}

impl Version {
    fn view(&self) -> Result<GraphVersion<'_, i64>, VersionError> {
        GraphVersion::new(&self.id_map, &self.relations, &self.graph)
    }
}

/// Builds the next version of `previous` from external-id node and edge
/// rows, as `refresh` would.
fn build(
    previous: &IdMap<i64>,
    relations: &RelationDictionary,
    nodes: Vec<i64>,
    edges: &[(u64, i64, i64)],
) -> Result<Version, GraphError> {
    let id_map = previous.rebuild(nodes)?;
    let mut relations = relations.clone();
    let mut intake = EdgeIntake::directed(&id_map, true);
    for &(relation, source, target) in edges {
        let relation = relations.intern(&format!("r{relation}"))?;
        intake.push(relation, Some(&source), Some(&target), 1.0)?;
    }
    let (list, _) = intake.finish();
    let graph = GraphCsr::build(list, relations.len(), id_map.id_space(), true)?;
    Ok(Version { id_map, relations, graph })
}

fn versions() -> Result<(Version, Version), GraphError> {
    let mut random = Lcg(0x2545_f491_4f6c_dd1d);
    let edge = |random: &mut Lcg| {
        let relation = random.next() % RELATIONS;
        (relation, (random.next() % NODES as u64) as i64, (random.next() % NODES as u64) as i64)
    };
    let mut edges: Vec<_> = (0..EDGES).map(|_| edge(&mut random)).collect();
    let before = build(&IdMap::new(), &RelationDictionary::new(), (0..NODES).collect(), &edges)?;
    for row in &mut edges {
        if random.next().is_multiple_of(20) {
            *row = edge(&mut random);
        }
    }
    let nodes = (NODES / 100..NODES + NODES / 100).collect();
    let after = build(&before.id_map, &before.relations, nodes, &edges)?;
    Ok((before, after))
}

fn bench_diff(c: &mut Criterion) {
    let (before, after) = match versions() {
        Ok(versions) => versions,
        Err(error) => {
            eprintln!("skipping version_diff: {error}");
            return;
        }
    };
    let (before, after) = match (before.view(), after.view()) {
        (Ok(before), Ok(after)) => (before, after),
        (Err(error), _) | (_, Err(error)) => {
            eprintln!("skipping version_diff: {error}");
            return;
        }
    };
    let mut group = c.benchmark_group("version_diff");
    group.throughput(Throughput::Elements(
        before.graph().all_relations().forward().edge_count()
            + after.graph().all_relations().forward().edge_count(),
    ));
    group.sample_size(10);
    group.bench_function("1m_edges_5_relations", |b| b.iter(|| diff(&before, &after)));
    group.finish();
}

criterion_group!(benches, bench_diff);
criterion_main!(benches);
