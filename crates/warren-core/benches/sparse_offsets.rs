//! DESIGN.md §5.3 sparse per-relation offsets (M11): relation-masked work
//! over one relation whose rows fill a fraction of the id space, with its
//! CSRs' offsets dense against sparse (binary search in the stored rows).
//!
//! `sparse_offsets/<nodes>/<fill>/<work>/<offsets>`: the relation has
//! `fill × nodes` stored rows of 8 weighted edges each, whose targets are
//! stored rows too (so a BFS stays in the relation); `spmv_forward` and
//! `spmv_reverse` pull over every row of the forward and reverse CSRs (a
//! lookup per node, most of them empty rows), `bfs_out` reads only rows
//! that exist, and `gather_sum` pulls one `f64` per stored row over the
//! reverse CSR. One thread. Bytes are printed once per graph.

use criterion::{criterion_group, criterion_main, Criterion};
use warren_core::compute::{
    gather, spmv, AnyVector, Combiner, ComputeMemory, Gather, Messages, Product, Semiring, Vector,
    Workers,
};
use warren_core::graph::{
    AnyCsr, Csr, CsrLayout, DenseId, Direction, EdgeList, GraphCsr, GraphError, RelationCsr,
    RelationDictionary, RelationIndex, RelationSelection,
};
use warren_core::interrupts::Uninterrupted;
use warren_core::traversal::{bfs, Adjacency, Limits};

const SIZES: [u64; 2] = [14_500, 1_000_000];
const FILLS: [f64; 6] = [0.001, 0.01, 0.1, 0.3, 0.6, 1.0];
const DEGREE: u64 = 8;

/// A relation over `nodes` whose stored rows are a `fill` of them.
fn edges(nodes: u64, fill: f64) -> Result<(EdgeList, RelationIndex), GraphError> {
    let mut dictionary = RelationDictionary::new();
    let relation = dictionary.intern("r")?;
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state =
            state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        state >> 33
    };
    let rows = ((nodes as f64 * fill) as u64).max(1);
    let stride = nodes / rows;
    let mut edges = EdgeList::for_id_space(nodes, true);
    for row in 0..rows {
        for _ in 0..DEGREE {
            let target = (next() % rows) * stride;
            edges.push(relation, row * stride, target, (next() % 100) as f64 / 10.0)?;
        }
    }
    Ok((edges, relation))
}

/// `csr` with dense offsets, or sparse ones, whichever the build chose.
fn with_offsets<T: warren_core::graph::Target>(
    csr: &Csr<T>,
    sparse: bool,
) -> Result<Csr<T>, GraphError> {
    let (mut rows, mut offsets, mut targets, mut weights) = (vec![], vec![0], vec![], vec![]);
    for source in 0..csr.id_space() {
        let row = csr.neighbours(source);
        if sparse && row.is_empty() {
            continue;
        }
        rows.extend(T::from_dense(source));
        targets.extend_from_slice(row);
        weights.extend_from_slice(csr.neighbour_weights(source).unwrap_or_default());
        offsets.push(targets.len() as u64);
    }
    // Without spare capacity, which `bytes` would count.
    offsets.shrink_to_fit();
    rows.shrink_to_fit();
    targets.shrink_to_fit();
    weights.shrink_to_fit();
    let weights = csr.is_weighted().then_some(weights);
    match sparse {
        true => Csr::from_sparse_parts(csr.id_space(), rows, offsets, targets, weights),
        false => Csr::from_parts(csr.id_space(), offsets, targets, weights),
    }
}

fn converted(csr: &AnyCsr, sparse: bool) -> Result<AnyCsr, GraphError> {
    Ok(match csr {
        AnyCsr::U32(csr) => AnyCsr::U32(with_offsets(csr, sparse)?),
        AnyCsr::U64(csr) => AnyCsr::U64(with_offsets(csr, sparse)?),
    })
}

/// The one-relation graph of `edges` with its offsets dense or sparse.
fn graph(edges: EdgeList, nodes: u64, sparse: bool) -> Result<GraphCsr, GraphError> {
    let layout = CsrLayout { build_reverse: true, store_merged: true };
    let built = GraphCsr::build_as(edges, 1, nodes, layout)?;
    let relation = &built.relations()[0];
    let reverse = relation.reverse().map(|reverse| converted(reverse, sparse)).transpose()?;
    let relation = RelationCsr::new(converted(relation.forward(), sparse)?, reverse)?;
    GraphCsr::from_parts(nodes, vec![relation], None, 0)
}

fn bench_sparse_offsets(c: &mut Criterion) {
    let Ok(workers) = Workers::new(1) else { return };
    let memory = ComputeMemory::new(u64::MAX);
    let mut never = Uninterrupted;
    let mut group = c.benchmark_group("sparse_offsets");
    group.sample_size(10);
    for nodes in SIZES {
        let vector = |value| Vector::filled(&memory, nodes, 1, value).map(AnyVector::from);
        let (Ok(x), Ok(mut y)) = (vector(1.0f64), vector(0.0f64)) else { continue };
        for fill in FILLS {
            let Ok((edges, relation)) = edges(nodes, fill) else { continue };
            let only = [relation];
            let selection = RelationSelection::Only(&only);
            let messages = Messages::<f64>::new(&memory).and_then(|mut messages| {
                let rows = ((nodes as f64 * fill) as u64).max(1);
                (0..rows).try_for_each(|row| messages.push(row * (nodes / rows), 1.0))?;
                Ok(messages)
            });
            let Ok(messages) = messages else { continue };
            let sum = Gather { relations: selection, ..Gather::new(Combiner::Sum) };
            for (name, sparse) in [("dense", false), ("sparse", true)] {
                let Ok(graph) = graph(edges.clone(), nodes, sparse) else { continue };
                let id = |work: &str| format!("{nodes}/{fill}/{work}/{name}");
                eprintln!(
                    "sparse_offsets {nodes} nodes, fill {fill}, {name}: {:.2} MB (forward + reverse)",
                    graph.bytes() as f64 / 1e6
                );
                for (work, transpose) in [("spmv_forward", false), ("spmv_reverse", true)] {
                    let product = Product {
                        transpose,
                        relations: selection,
                        ..Product::new(Semiring::PlusTimes)
                    };
                    group.bench_function(id(work), |b| {
                        b.iter(|| spmv(&workers, &memory, &mut never, &graph, &product, &x, &mut y))
                    });
                }
                let Ok(adjacency) = Adjacency::new(&graph, Direction::Out, selection) else {
                    continue;
                };
                let start: [DenseId; 1] = [0];
                group.bench_function(id("bfs_out"), |b| {
                    b.iter(|| bfs(&adjacency, &start, None, Limits::default(), &mut never))
                });
                group.bench_function(id("gather_sum"), |b| {
                    b.iter(|| {
                        gather(&workers, &memory, &mut never, &graph, &sum, &messages)
                            .map(|g| g.len())
                    })
                });
            }
        }
    }
    group.finish();
}

criterion_group!(benches, bench_sparse_offsets);
criterion_main!(benches);
