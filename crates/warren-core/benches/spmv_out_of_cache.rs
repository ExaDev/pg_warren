//! SpMV throughput on graphs far beyond a core's L2 (DESIGN.md §6.3,
//! §6.10): `plus_times` pulled over the reverse CSR, as `pagerank`'s
//! product runs, on a G(n, m) random graph and on a power-law graph in
//! random and in hub-first node order, `f64` and `f32`, one thread and
//! every core. Beside it, the per-iteration vector work of `pagerank`
//! (its four maps, a sum, an L1 `norm_diff` and `y`'s fill) at the same size.

use std::thread::available_parallelism;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use warren_core::compute::{
    map, norm_diff, reduce, AnyExpr, AnyVector, Arithmetic, BoolOp, Comparands, Comparison,
    ComputeMemory, Expr, F64Op, Norm, Product, Reduction, Semiring, Vector, Workers,
};
use warren_core::graph::{EdgeList, GraphCsr, GraphError, RelationDictionary};
use warren_core::interrupts::Uninterrupted;

const ID_SPACE: u64 = 4_000_000;
const EDGES: u64 = 40_000_000;

/// A 64-bit LCG's high bits, fixed so runs are comparable.
fn lcg() -> impl FnMut() -> u64 {
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    move || {
        state =
            state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        state >> 11
    }
}

/// How the benchmark graph's endpoints are drawn.
#[derive(Clone, Copy)]
enum Shape {
    /// Uniform: G(n, m).
    Uniform,
    /// Skewed towards low ids (`n · u³`, a power-law degree tail), so hubs
    /// come first.
    HubsFirst,
    /// The same skewed graph with its ids randomly permuted.
    PowerLaw,
}

fn edges(shape: Shape) -> Result<EdgeList, GraphError> {
    let relation = RelationDictionary::new().intern("edge")?;
    let mut next = lcg();
    let mut permutation: Vec<u64> = (0..ID_SPACE).collect();
    if let Shape::PowerLaw = shape {
        for i in (1..permutation.len()).rev() {
            permutation.swap(i, (next() % (i as u64 + 1)) as usize);
        }
    }
    let mut endpoint = move || {
        let u = next() as f64 / (1u64 << 53) as f64;
        match shape {
            Shape::Uniform => (u * ID_SPACE as f64) as u64,
            Shape::HubsFirst | Shape::PowerLaw => {
                permutation[(u * u * u * ID_SPACE as f64) as usize]
            }
        }
    };
    let mut edges = EdgeList::for_id_space(ID_SPACE, false);
    edges.reserve(EDGES as usize)?;
    for _ in 0..EDGES {
        let (source, target) = (endpoint(), endpoint());
        edges.push(relation, source, target, 1.0)?;
    }
    Ok(edges)
}

fn bench_spmv(c: &mut Criterion) {
    let memory = ComputeMemory::new(u64::MAX);
    let cores = available_parallelism().map_or(1, |n| n.get());
    let mut group = c.benchmark_group("spmv_out_of_cache");
    group.throughput(Throughput::Elements(EDGES));
    group.sample_size(10);
    for (name, shape) in [
        ("uniform", Shape::Uniform),
        ("power_law", Shape::PowerLaw),
        ("hubs_first", Shape::HubsFirst),
    ] {
        let Ok(graph) = edges(shape).and_then(|e| GraphCsr::build(e, 1, ID_SPACE, true)) else {
            eprintln!("skipping {name}: graph build failed");
            continue;
        };
        let f64s = |value| Vector::filled(&memory, ID_SPACE, 1, value).map(AnyVector::from);
        let f32s = |value| Vector::filled(&memory, ID_SPACE, 1, value).map(AnyVector::from);
        let mut operands = [("f64", f64s(1.0f64), f64s(0.0)), ("f32", f32s(1.0f32), f32s(0.0))];
        for threads in [1, cores] {
            let Ok(workers) = Workers::new(threads) else { continue };
            for (dtype, x, y) in &mut operands {
                let (Ok(x), Ok(y)) = (x, y) else { continue };
                let product = Product { transpose: true, ..Product::new(Semiring::PlusTimes) };
                group.bench_function(format!("{name}/{dtype}/threads_{threads}"), |b| {
                    b.iter(|| {
                        warren_core::compute::spmv(
                            &workers,
                            &memory,
                            &mut Uninterrupted,
                            &graph,
                            &product,
                            x,
                            y,
                        )
                    })
                });
            }
        }
    }
    group.finish();
}

/// `pagerank`'s maps (`pagerank.sql`), over vectors `[x, out, y, p,
/// dangling, damping, teleport]`: `msg`, `dangling_x`, `next` and the
/// copy of `next` into `x`.
fn pagerank_maps() -> [(&'static str, AnyExpr); 4] {
    let vector = |i| Expr::Vector(i);
    let op = |a, l, r| Expr::Op(F64Op::Arithmetic(a, Box::new(l), Box::new(r)));
    let has_out = || {
        let zero = Expr::Literal(Some(0.0));
        Expr::Op(BoolOp::Compare(Comparison::Gt, Box::new(Comparands::F64(vector(1), zero))))
    };
    let case = |then, otherwise| Expr::Case {
        when: vec![(has_out(), then)],
        otherwise: Some(Box::new(otherwise)),
    };
    let zero = || Expr::Literal(Some(0.0));
    let msg = case(op(Arithmetic::Divide, vector(0), vector(1)), zero());
    let dangling_x = case(zero(), vector(0));
    let dangling = op(Arithmetic::Multiply, vector(4), vector(3));
    let damped = op(Arithmetic::Multiply, vector(5), op(Arithmetic::Add, vector(2), dangling));
    let next = op(Arithmetic::Add, damped, op(Arithmetic::Multiply, vector(6), vector(3)));
    [
        ("map_msg", AnyExpr::F64(msg)),
        ("map_dangling_x", AnyExpr::F64(dangling_x)),
        ("map_next", AnyExpr::F64(next)),
        ("map_copy", AnyExpr::F64(vector(0))),
    ]
}

fn bench_vector_work(c: &mut Criterion) {
    let memory = ComputeMemory::new(u64::MAX);
    let cores = available_parallelism().map_or(1, |n| n.get());
    let mut group = c.benchmark_group("pagerank_vector_work");
    group.throughput(Throughput::Elements(ID_SPACE));
    group.sample_size(10);
    let vector = |value| Vector::filled(&memory, ID_SPACE, 1, value).map(AnyVector::from);
    let values = [0.25f64, 10.0, 0.5, 0.25, 0.1, 0.85, 0.15];
    let Ok(operands) = values.map(vector).into_iter().collect::<Result<Vec<_>, _>>() else {
        eprintln!("skipping vector work: allocation failed");
        return;
    };
    let operands: Vec<&AnyVector> = operands.iter().collect();
    for threads in [1, cores] {
        let Ok(workers) = Workers::new(threads) else { continue };
        for (name, expr) in &pagerank_maps() {
            group.bench_function(format!("{name}/threads_{threads}"), |b| {
                b.iter(|| map(&workers, &memory, &mut Uninterrupted, ID_SPACE, expr, &operands))
            });
        }
        group.bench_function(format!("reduce_sum/threads_{threads}"), |b| {
            b.iter(|| reduce(&workers, &mut Uninterrupted, operands[0], Reduction::Sum))
        });
        group.bench_function(format!("norm_diff_l1/threads_{threads}"), |b| {
            b.iter(|| norm_diff(&workers, &mut Uninterrupted, operands[0], operands[2], Norm::L1))
        });
        group.bench_function(format!("filled_y/threads_{threads}"), |b| b.iter(|| vector(0.0)));
    }
    group.finish();
}

criterion_group!(benches, bench_spmv, bench_vector_work);
criterion_main!(benches);
