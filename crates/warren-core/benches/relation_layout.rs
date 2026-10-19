//! DESIGN.md §5.3 layout benchmark: per-relation CSRs (plus the merged CSR)
//! against one CSR with a parallel `u8` relation array (pgGraph's layout).
//!
//! Five relations, ~1M weighted edges over 100K nodes. Measures build cost
//! (forward + reverse), and relation-masked work over 1, 2, 3 and all 5
//! relations: a BFS from one node and an SpMV-like weighted row sum. Memory
//! is computed from the segment sizes and printed once.
//!
//! The parallel-relation layout lives only here, as the comparison baseline.

use std::collections::VecDeque;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use warren_core::graph::{AnyCsr, Csr, EdgeList, GraphCsr, GraphError, RelationDictionary};

const ID_SPACE: u64 = 100_000;
const EDGES: u64 = 1_000_000;
const RELATIONS: usize = 5;
const MASKS: [&[usize]; 4] = [&[0], &[0, 1], &[0, 1, 2], &[0, 1, 2, 3, 4]];

fn random_edges() -> Result<EdgeList, GraphError> {
    let mut dictionary = RelationDictionary::new();
    let relations = (0..RELATIONS)
        .map(|r| dictionary.intern(&format!("r{r}")))
        .collect::<Result<Vec<_>, _>>()?;
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state =
            state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        state >> 33
    };
    let mut edges = EdgeList::new(true);
    for _ in 0..EDGES {
        let relation = relations[next() as usize % RELATIONS];
        let (source, target) = (next() % ID_SPACE, next() % ID_SPACE);
        edges.push(relation, source, target, (next() % 100) as f64 / 10.0)?;
    }
    Ok(edges)
}

/// One CSR whose entries carry a relation id; rows sorted by (target, relation).
struct Tagged {
    offsets: Vec<u64>,
    targets: Vec<u32>,
    relations: Vec<u8>,
    weights: Vec<f64>,
}

impl Tagged {
    fn bytes(&self) -> usize {
        self.offsets.len() * 8 + self.targets.len() * (4 + 1 + 8)
    }

    fn row(&self, source: usize) -> std::ops::Range<usize> {
        self.offsets[source] as usize..self.offsets[source + 1] as usize
    }

    /// Counting sort by source, then sort and deduplicate each row by
    /// (target, relation) — the same technique as `GraphCsr::build`.
    fn build(edges: &EdgeList) -> Self {
        let rows = ID_SPACE as usize;
        let mut offsets = vec![0u64; rows + 1];
        for &source in edges.sources() {
            offsets[source as usize + 1] += 1;
        }
        for row in 1..=rows {
            offsets[row] += offsets[row - 1];
        }
        let mut cursor = offsets[..rows].to_vec();
        let mut entries = vec![(0u32, 0u8, 0.0f64); edges.len()];
        let weights = edges.weights().unwrap_or(&[]);
        for edge in 0..edges.len() {
            let source = edges.sources()[edge] as usize;
            let position = cursor[source] as usize;
            cursor[source] += 1;
            let weight = weights.get(edge).copied().unwrap_or(1.0);
            entries[position] =
                (edges.targets()[edge] as u32, edges.relations()[edge].index() as u8, weight);
        }
        let mut tagged = Self {
            offsets: vec![0; rows + 1],
            targets: Vec::with_capacity(entries.len()),
            relations: Vec::with_capacity(entries.len()),
            weights: Vec::with_capacity(entries.len()),
        };
        for source in 0..rows {
            let row = &mut entries[offsets[source] as usize..offsets[source + 1] as usize];
            row.sort_by_key(|&(target, relation, _)| (target, relation));
            let start = tagged.targets.len();
            for &(target, relation, weight) in row.iter() {
                let last = tagged.targets.len().checked_sub(1).filter(|&i| i >= start);
                match last {
                    Some(i) if (tagged.targets[i], tagged.relations[i]) == (target, relation) => {
                        tagged.weights[i] += weight;
                    }
                    _ => {
                        tagged.targets.push(target);
                        tagged.relations.push(relation);
                        tagged.weights.push(weight);
                    }
                }
            }
            tagged.offsets[source + 1] = tagged.targets.len() as u64;
        }
        tagged
    }

    fn transpose(&self) -> Self {
        let rows = self.offsets.len() - 1;
        let mut offsets = vec![0u64; rows + 1];
        for &target in &self.targets {
            offsets[target as usize + 1] += 1;
        }
        for row in 1..=rows {
            offsets[row] += offsets[row - 1];
        }
        let mut cursor = offsets[..rows].to_vec();
        let edges = self.targets.len();
        let mut reverse = Self {
            offsets,
            targets: vec![0; edges],
            relations: vec![0; edges],
            weights: vec![0.0; edges],
        };
        for source in 0..rows {
            for entry in self.row(source) {
                let target = self.targets[entry] as usize;
                let position = cursor[target] as usize;
                cursor[target] += 1;
                reverse.targets[position] = source as u32;
                reverse.relations[position] = self.relations[entry];
                reverse.weights[position] = self.weights[entry];
            }
        }
        reverse
    }
}

fn narrow(csr: &AnyCsr) -> Option<&Csr<u32>> {
    match csr {
        AnyCsr::U32(csr) => Some(csr),
        AnyCsr::U64(_) => None,
    }
}

fn csr_bytes(csr: &Csr<u32>) -> usize {
    csr.offsets().len() * 8 + csr.targets().len() * (4 + 8)
}

/// The CSRs a masked traversal reads: the merged CSR for all relations,
/// else one per relation in the mask.
fn per_relation_csrs<'g>(graph: &'g GraphCsr, mask: &[usize]) -> Vec<&'g Csr<u32>> {
    if mask.len() == RELATIONS {
        return narrow(graph.all_relations().forward()).into_iter().collect();
    }
    mask.iter()
        .filter_map(|&r| graph.relations().get(r))
        .filter_map(|relation| narrow(relation.forward()))
        .collect()
}

fn bfs_per_relation(csrs: &[&Csr<u32>]) -> usize {
    let mut visited = vec![false; ID_SPACE as usize];
    let mut queue = VecDeque::from([0u32]);
    visited[0] = true;
    let mut reached = 0;
    while let Some(node) = queue.pop_front() {
        reached += 1;
        for csr in csrs {
            for &target in csr.neighbours(u64::from(node)) {
                if !std::mem::replace(&mut visited[target as usize], true) {
                    queue.push_back(target);
                }
            }
        }
    }
    reached
}

fn bfs_tagged(tagged: &Tagged, mask: u8) -> usize {
    let mut visited = vec![false; ID_SPACE as usize];
    let mut queue = VecDeque::from([0u32]);
    visited[0] = true;
    let mut reached = 0;
    while let Some(node) = queue.pop_front() {
        reached += 1;
        for entry in tagged.row(node as usize) {
            if mask & (1 << tagged.relations[entry]) == 0 {
                continue;
            }
            let target = tagged.targets[entry];
            if !std::mem::replace(&mut visited[target as usize], true) {
                queue.push_back(target);
            }
        }
    }
    reached
}

fn row_sums_per_relation(csrs: &[&Csr<u32>], x: &[f64], y: &mut [f64]) {
    y.fill(0.0);
    for csr in csrs {
        let weights = csr.weights().unwrap_or(&[]);
        for (source, sum) in y.iter_mut().enumerate() {
            let row = csr.offsets()[source] as usize..csr.offsets()[source + 1] as usize;
            for (target, weight) in csr.targets()[row.clone()].iter().zip(&weights[row]) {
                *sum += weight * x[*target as usize];
            }
        }
    }
}

fn row_sums_tagged(tagged: &Tagged, mask: u8, x: &[f64], y: &mut [f64]) {
    for (source, sum) in y.iter_mut().enumerate() {
        let mut total = 0.0;
        for entry in tagged.row(source) {
            if mask & (1 << tagged.relations[entry]) != 0 {
                total += tagged.weights[entry] * x[tagged.targets[entry] as usize];
            }
        }
        *sum = total;
    }
}

fn bench_relation_layout(c: &mut Criterion) {
    let built = random_edges().and_then(|edges| {
        GraphCsr::build(edges.clone(), RELATIONS, ID_SPACE, true).map(|graph| (edges, graph))
    });
    let (edges, graph) = match built {
        Ok(built) => built,
        Err(error) => {
            eprintln!("skipping relation_layout: {error}");
            return;
        }
    };
    let tagged = Tagged::build(&edges);
    let tagged_reverse = tagged.transpose();

    let forward_bytes: usize = graph
        .relations()
        .iter()
        .chain([graph.all_relations()])
        .filter_map(|relation| narrow(relation.forward()))
        .map(csr_bytes)
        .sum();
    eprintln!(
        "relation_layout memory (forward; reverse is the same again): per-relation + merged {:.1} MB, \
         per-relation alone {:.1} MB, parallel relation array {:.1} MB",
        forward_bytes as f64 / 1e6,
        (forward_bytes - narrow(graph.all_relations().forward()).map_or(0, csr_bytes)) as f64 / 1e6,
        tagged.bytes() as f64 / 1e6,
    );
    eprintln!(
        "relation_layout reverse built: {} / {} entries",
        tagged_reverse.targets.len(),
        graph.all_relations().reverse().map_or(0, AnyCsr::edge_count)
    );

    let mut build = c.benchmark_group("relation_layout/build");
    build.sample_size(10);
    build.bench_function("per_relation_plus_merged", |b| {
        b.iter_batched(
            || edges.clone(),
            |edges| GraphCsr::build(edges, RELATIONS, ID_SPACE, true),
            BatchSize::LargeInput,
        )
    });
    build.bench_function("parallel_relation_array", |b| {
        b.iter(|| {
            let tagged = Tagged::build(&edges);
            let reverse = tagged.transpose();
            (tagged, reverse)
        })
    });
    build.finish();

    let x: Vec<f64> = (0..ID_SPACE).map(|i| (i % 7) as f64).collect();
    let mut y = vec![0.0; ID_SPACE as usize];
    let mut scans = c.benchmark_group("relation_layout/masked");
    scans.sample_size(20);
    for mask in MASKS {
        let csrs = per_relation_csrs(&graph, mask);
        let bits = mask.iter().fold(0u8, |bits, &r| bits | 1 << r);
        let k = mask.len();
        eprintln!(
            "relation_layout {k}_of_5 BFS reach: per-relation {}, parallel relation array {}",
            bfs_per_relation(&csrs),
            bfs_tagged(&tagged, bits)
        );
        scans.bench_function(format!("bfs/{k}_of_5/per_relation"), |b| {
            b.iter(|| bfs_per_relation(&csrs))
        });
        scans.bench_function(format!("bfs/{k}_of_5/parallel_relation_array"), |b| {
            b.iter(|| bfs_tagged(&tagged, bits))
        });
        scans.bench_function(format!("row_sums/{k}_of_5/per_relation"), |b| {
            b.iter(|| row_sums_per_relation(&csrs, &x, &mut y))
        });
        scans.bench_function(format!("row_sums/{k}_of_5/parallel_relation_array"), |b| {
            b.iter(|| row_sums_tagged(&tagged, bits, &x, &mut y))
        });
    }
    scans.finish();
}

criterion_group!(benches, bench_relation_layout);
criterion_main!(benches);
