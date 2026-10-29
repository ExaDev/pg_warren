use std::sync::OnceLock;

use proptest::prelude::*;

use super::*;
use crate::compute::map::float;
use crate::compute::reference::{self, Edges};
use crate::compute::{
    map, norm_diff, reduce_live, spmv, AnyExpr, Arithmetic, BoolOp, Comparands, Comparison, Dtype,
    Element, Expr, F64Op, Norm, Product, Reduction, Semiring,
};
use crate::graph::{CsrLayout, RelationIndex};
use crate::interrupts::{Cancelled, Uninterrupted};
use crate::progress::recorded::Recorded;

fn workers() -> &'static Workers {
    static WORKERS: OnceLock<Workers> = OnceLock::new();
    WORKERS.get_or_init(|| Workers::new(3).unwrap())
}

fn memory() -> ComputeMemory {
    ComputeMemory::new(1 << 24)
}

/// NetworkX's defaults: damping 0.85, tol 1e-6, 100 iterations, uniform.
fn defaults() -> PageRank<'static> {
    PageRank {
        relations: RelationSelection::All,
        weighted: None,
        per_relation: false,
        damping: 0.85,
        tol: Some(1e-6),
        max_iter: 100,
        seed: None,
    }
}

fn ranked(
    graph: &GraphCsr,
    tombstones: &Tombstones,
    pagerank: &PageRank<'_>,
) -> Result<(Vec<f64>, u32), ComputeError> {
    let ranks =
        super::pagerank(workers(), &memory(), &mut Uninterrupted, graph, tombstones, pagerank)?;
    Ok((ranks.values.values().to_vec(), ranks.iterations))
}

fn f64_vector(values: Vec<f64>) -> AnyVector {
    let id_space = values.len() as u64;
    Vector::from_values(&memory(), id_space, 1, values).unwrap().into()
}

// The fixture `small` of the extension's tests, one more edge: nodes 1–6
// as dense ids 0–5, relations a (0) and b (1), weighted; node 6 has no
// edge.
fn small() -> Edges {
    vec![
        (0, 0, 1, 1.0),
        (0, 1, 2, 1.0),
        (1, 2, 3, 1.0),
        (1, 0, 3, 5.0),
        (0, 3, 0, 1.0),
        (1, 4, 4, 2.0),
        (1, 0, 1, 0.5),
    ]
}

/// `node:value` pairs, as `warren.vector_values` lists the live ones.
fn listed(values: &[f64], tombstones: &Tombstones) -> Vec<(u64, f64)> {
    (0..values.len() as u64)
        .filter(|&node| !tombstones.contains(node))
        .map(|node| (node + 1, values[node as usize]))
        .collect()
}

/// The ranks pg_warren 0.7.0's PL/pgSQL `warren.pagerank` wrote for the
/// same graphs (release build, PG 16), to the last bit.
#[test]
fn ranks_are_those_the_plpgsql_library_wrote() {
    let graph = reference::graph(6, 2, &small(), true, true);
    let none = Tombstones::new(6).unwrap();
    let (values, iterations) = ranked(&graph, &none, &defaults()).unwrap();
    assert_eq!(iterations, 14);
    let expected = [
        0.286_669_446_589_697_95,
        0.085_357_179_719_747_21,
        0.101_680_067_973_336_76,
        0.302_992_334_843_287_5,
        0.194_174_757_281_517_29,
        0.029_126_213_592_413_387,
    ];
    assert_eq!(values, expected);

    // Node 2 deleted: its dense id tombstoned, its edges gone.
    let edges: Edges = small().into_iter().filter(|&(_, s, t, _)| s != 1 && t != 1).collect();
    let graph = reference::graph(6, 2, &edges, true, true);
    let mut tombstones = Tombstones::new(6).unwrap();
    tombstones.insert(1).unwrap();
    let (values, iterations) = ranked(&graph, &tombstones, &defaults()).unwrap();
    assert_eq!(iterations, 69);
    let expected = [
        (1, 0.335_068_977_003_068_37),
        (3, 0.036_144_578_313_253_02),
        (4, 0.351_678_010_948_739_1),
        (5, 0.240_963_855_421_686_75),
        (6, 0.036_144_578_313_253_02),
    ];
    assert_eq!(listed(&values, &tombstones), expected);
    assert_eq!(values[1], 0.0);

    let seed = f64_vector((1..=6).map(f64::from).collect());
    let personalised = PageRank { seed: Some(&seed), ..defaults() };
    let (values, iterations) = ranked(&graph, &tombstones, &personalised).unwrap();
    assert_eq!(iterations, 71);
    let expected = [
        (1, 0.255_395_233_723_480_8),
        (3, 0.032_374_100_719_424_46),
        (4, 0.287_771_283_271_850_25),
        (5, 0.359_711_180_846_395_6),
        (6, 0.064_748_201_438_848_92),
    ];
    assert_eq!(listed(&values, &tombstones), expected);
}

#[test]
fn a_ring_ranks_every_node_alike_and_a_seed_ranks_itself_first() {
    let edges: Edges = (0..4).map(|n| (0, n, (n + 1) % 4, 1.0)).collect();
    let graph = reference::graph(4, 1, &edges, false, true);
    let none = Tombstones::new(4).unwrap();
    let exact = PageRank { tol: Some(1e-12), ..defaults() };
    let (values, _) = ranked(&graph, &none, &exact).unwrap();
    assert!(values.iter().all(|&rank| (rank - 0.25).abs() < 1e-12), "{values:?}");
    let seed = f64_vector(vec![1.0, 0.0, 0.0, 0.0]);
    let (values, _) = ranked(&graph, &none, &PageRank { seed: Some(&seed), ..exact }).unwrap();
    assert!((values.iter().sum::<f64>() - 1.0).abs() < 1e-12);
    assert!(values[0] > values[1] && values[1] > values[2] && values[2] > values[3], "{values:?}");
}

#[test]
fn no_iterations_leave_the_uniform_start_and_no_tol_runs_them_all() {
    let graph = reference::graph(6, 2, &small(), true, true);
    let none = Tombstones::new(6).unwrap();
    let (values, iterations) =
        ranked(&graph, &none, &PageRank { max_iter: 0, ..defaults() }).unwrap();
    assert_eq!((values, iterations), (vec![1.0 / 6.0; 6], 0));
    let (_, iterations) =
        ranked(&graph, &none, &PageRank { tol: None, max_iter: 7, ..defaults() }).unwrap();
    assert_eq!(iterations, 7);
    // NaN sorts above every number in SQL, so every change is below it.
    let (_, iterations) =
        ranked(&graph, &none, &PageRank { tol: Some(f64::NAN), ..defaults() }).unwrap();
    assert_eq!(iterations, 1);
}

#[test]
fn what_cannot_be_divided_is_refused_as_sql_refuses_it() {
    let graph = reference::graph(3, 1, &vec![(0, 0, 1, 1.0)], false, true);
    let mut tombstones = Tombstones::new(3).unwrap();
    for node in 0..3 {
        tombstones.insert(node).unwrap();
    }
    let division = ComputeError::Arithmetic(crate::compute::ArithmeticError::DivisionByZero);
    let refused = ranked(&graph, &tombstones, &PageRank { max_iter: 0, ..defaults() });
    assert_eq!(refused.unwrap_err(), division, "no live nodes");
    let none = Tombstones::new(3).unwrap();
    let zero = f64_vector(vec![1.0, -1.0, 0.0]);
    let refused = ranked(&graph, &none, &PageRank { seed: Some(&zero), ..defaults() });
    assert_eq!(refused.unwrap_err(), division, "a seed summing to zero");
}

#[test]
fn a_seed_must_be_one_numeric_column() {
    let graph = reference::graph(3, 1, &vec![(0, 0, 1, 1.0)], false, true);
    let none = Tombstones::new(3).unwrap();
    let wide: AnyVector = Vector::from_values(&memory(), 3, 2, vec![1.0; 6]).unwrap().into();
    let refused = ranked(&graph, &none, &PageRank { seed: Some(&wide), ..defaults() });
    assert_eq!(refused.unwrap_err(), ComputeError::WidthMismatch { expected: 2, found: 1 });
    // As the PL/pgSQL library: the widths meet only in an iteration.
    let idle = PageRank { seed: Some(&wide), max_iter: 0, ..defaults() };
    assert_eq!(ranked(&graph, &none, &idle).unwrap().1, 0);
    let flags: AnyVector = Vector::from_values(&memory(), 3, 1, vec![true; 3]).unwrap().into();
    let refused = ranked(&graph, &none, &PageRank { seed: Some(&flags), ..defaults() });
    assert_eq!(
        refused.unwrap_err(),
        ComputeError::DtypeMismatch { expected: Dtype::F64, found: Dtype::Bool }
    );
}

#[test]
fn iterations_are_reported_as_they_start() {
    let graph = reference::graph(6, 2, &small(), true, true);
    let none = Tombstones::new(6).unwrap();
    let mut recorded = Recorded::default();
    let pagerank = PageRank { tol: None, max_iter: 3, ..defaults() };
    super::pagerank(workers(), &memory(), &mut recorded, &graph, &none, &pagerank).unwrap();
    assert_eq!(recorded.phases(), [("iterating", Some(3))]);
    assert_eq!(recorded.checked_done(), [2]);
    let mut recorded = Recorded::default();
    let idle = PageRank { max_iter: 0, ..defaults() };
    super::pagerank(workers(), &memory(), &mut recorded, &graph, &none, &idle).unwrap();
    assert!(recorded.0.is_empty(), "{:?}", recorded.0);
}

/// Interrupts that stop the work at the first poll.
struct Stop;

impl Interrupts for Stop {
    fn check(&mut self) -> Result<(), Cancelled> {
        Err(Cancelled)
    }
}

#[test]
fn a_cancel_or_a_spent_budget_stops_it() {
    let graph = reference::graph(6, 2, &small(), true, true);
    let none = Tombstones::new(6).unwrap();
    let stopped = super::pagerank(workers(), &memory(), &mut Stop, &graph, &none, &defaults());
    assert_eq!(stopped.unwrap_err(), ComputeError::Cancelled);
    let tight = ComputeMemory::new(100);
    let refused =
        super::pagerank(workers(), &tight, &mut Uninterrupted, &graph, &none, &defaults());
    assert!(matches!(refused, Err(ComputeError::MemoryLimit { .. })), "{refused:?}");
}

/// NetworkX's `pagerank`, densely: the ranks of the live nodes `live`
/// under adjacency `a`, after `iterations` steps.
fn dense(a: &[Vec<Option<f64>>], live: &[bool], damping: f64, iterations: u32) -> Vec<f64> {
    let n = a.len();
    let nodes = live.iter().filter(|&&live| live).count() as f64;
    let uniform = |i: usize| if live[i] { 1.0 / nodes } else { 0.0 };
    let out: Vec<f64> = a.iter().map(|row| row.iter().flatten().sum()).collect();
    let mut x: Vec<f64> = (0..n).map(uniform).collect();
    for _ in 0..iterations {
        let dangling: f64 = (0..n).filter(|&i| out[i] <= 0.0).map(|i| x[i]).sum();
        x = (0..n)
            .map(|j| match live[j] {
                false => 0.0,
                true => {
                    let inflow: f64 = (0..n)
                        .filter_map(|i| a[i][j].filter(|_| out[i] > 0.0).map(|w| w * x[i] / out[i]))
                        .sum();
                    damping * (inflow + dangling * uniform(j)) + (1.0 - damping) * uniform(j)
                }
            })
            .collect();
    }
    x
}

#[test]
fn ranks_agree_with_a_dense_power_iteration() {
    let edges: Edges = vec![
        (0, 0, 1, 2.0),
        (0, 0, 2, 1.0),
        (0, 1, 2, 3.0),
        (0, 2, 0, 1.0),
        (0, 3, 2, 0.5),
        (0, 4, 4, 1.0),
    ];
    let graph = reference::graph(6, 1, &edges, true, false);
    let mut tombstones = Tombstones::new(6).unwrap();
    tombstones.insert(5).unwrap();
    let a = reference::adjacency(6, &edges, None, true);
    let live = [true, true, true, true, true, false];
    let pagerank = PageRank { tol: None, max_iter: 30, ..defaults() };
    let (values, _) = ranked(&graph, &tombstones, &pagerank).unwrap();
    let expected = dense(&a, &live, 0.85, 30);
    for (found, expected) in values.iter().zip(&expected) {
        assert!((found - expected).abs() < 1e-14, "{values:?} against {expected:?}");
    }
}

/// The library's PL/pgSQL `warren.pagerank` (0.7.0), call for call over
/// the primitives it was written with: `vector_const` (zero at tombstoned
/// rows), `spmv`, `vector_reduce` (live rows), `vector_map` (its result's
/// tombstoned rows cleared) and `vector_norm_diff`.
struct Program<'a> {
    graph: &'a GraphCsr,
    tombstones: &'a Tombstones,
    pagerank: &'a PageRank<'a>,
    memory: ComputeMemory,
}

impl Program<'_> {
    fn id_space(&self) -> u64 {
        self.graph.all_relations().forward().id_space()
    }

    fn constant(&self, value: f64) -> AnyVector {
        let mut vector = Vector::filled(&self.memory, self.id_space(), 1, value).unwrap();
        vector.clear_tombstoned(self.tombstones).unwrap();
        vector.into()
    }

    fn map(&self, expr: Expr<f64>, operands: &[&AnyVector]) -> Result<AnyVector, ComputeError> {
        let expr = AnyExpr::F64(expr);
        let id_space = self.id_space();
        let mapped = map(workers(), &self.memory, &mut Uninterrupted, id_space, &expr, operands)?;
        let AnyVector::F64(mut vector) = mapped else { panic!("an f64 expression") };
        vector.clear_tombstoned(self.tombstones)?;
        Ok(vector.into())
    }

    fn spmv(
        &self,
        semiring: Semiring,
        transpose: bool,
        x: &AnyVector,
    ) -> Result<AnyVector, ComputeError> {
        let pagerank = self.pagerank;
        let product = Product {
            transpose,
            relations: pagerank.relations,
            weighted: pagerank.weighted,
            per_relation: pagerank.per_relation,
            ..Product::new(semiring)
        };
        let mut y = self.constant(0.0);
        spmv(workers(), &self.memory, &mut Uninterrupted, self.graph, &product, x, &mut y)?;
        let AnyVector::F64(mut vector) = y else { panic!("an f64 product") };
        vector.clear_tombstoned(self.tombstones)?;
        Ok(vector.into())
    }

    fn sum(&self, x: &AnyVector) -> Result<f64, ComputeError> {
        let sum = reduce_live(workers(), &mut Uninterrupted, x, self.tombstones, Reduction::Sum)?;
        Ok(sum.unwrap())
    }

    fn run(&self) -> Result<(Vec<f64>, u32), ComputeError> {
        use Expr::{Literal, Op, Vector as V};
        let arithmetic = |op, a, b| Op(F64Op::Arithmetic(op, Box::new(a), Box::new(b)));
        let positive = |index| {
            let comparands = Comparands::F64(V(index), Literal(Some(0.0)));
            Op(BoolOp::Compare(Comparison::Gt, Box::new(comparands)))
        };
        let pagerank = self.pagerank;
        let one = self.constant(1.0);
        let out = self.spmv(Semiring::PlusFirst, false, &one)?;
        let nodes = self.sum(&one)?;
        let mut x = self.constant(float(Arithmetic::Divide, 1.0, nodes)?);
        let p = match pagerank.seed {
            None => self.map(V(0), &[&x])?,
            Some(seed) => {
                let total = self.constant(float(Arithmetic::Divide, 1.0, self.sum(seed)?)?);
                let seed_value = match seed.dtype() {
                    Dtype::F64 => V(0),
                    Dtype::F32 => Expr::Cast(Box::new(AnyExpr::F32(V(0)))),
                    Dtype::I64 => Expr::Cast(Box::new(AnyExpr::I64(V(0)))),
                    Dtype::Bool => panic!("bool * double precision is no operator"),
                };
                self.map(arithmetic(Arithmetic::Multiply, seed_value, V(1)), &[seed, &total])?
            }
        };
        let damping = self.constant(pagerank.damping);
        let teleport = self.constant(float(Arithmetic::Subtract, 1.0, pagerank.damping)?);
        let mut i = 0;
        while i < pagerank.max_iter {
            i += 1;
            let msg = Expr::Case {
                when: vec![(positive(1), arithmetic(Arithmetic::Divide, V(0), V(1)))],
                otherwise: Some(Box::new(Literal(Some(0.0)))),
            };
            let msg = self.map(msg, &[&x, &out])?;
            let dangling_x = Expr::Case {
                when: vec![(positive(1), Literal(Some(0.0)))],
                otherwise: Some(Box::new(V(0))),
            };
            let dangling_x = self.map(dangling_x, &[&x, &out])?;
            let dangling = self.constant(self.sum(&dangling_x)?);
            let y = self.spmv(Semiring::PlusTimes, true, &msg)?;
            // damping * (y + dangling * p) + teleport * p
            let flowed =
                arithmetic(Arithmetic::Add, V(1), arithmetic(Arithmetic::Multiply, V(2), V(3)));
            let next = arithmetic(
                Arithmetic::Add,
                arithmetic(Arithmetic::Multiply, V(0), flowed),
                arithmetic(Arithmetic::Multiply, V(4), V(3)),
            );
            let next = self.map(next, &[&damping, &y, &dangling, &p, &teleport])?;
            let delta = norm_diff(workers(), &mut Uninterrupted, &next, &x, Norm::L1)?;
            x = self.map(V(0), &[&next])?;
            if let Some(tol) = pagerank.tol {
                if delta.sql_cmp(float(Arithmetic::Multiply, nodes, tol)?).is_lt() {
                    break;
                }
            }
        }
        let AnyVector::F64(x) = x else { panic!("f64 ranks") };
        Ok((x.values().to_vec(), i))
    }
}

/// A random graph, its tombstones and a PageRank over it.
#[derive(Debug, Clone)]
struct Case {
    id_space: u64,
    relation_count: usize,
    edges: Edges,
    tombstoned: Vec<u64>,
    layout: CsrLayout,
    selected: Option<Vec<usize>>,
    weighted: Option<bool>,
    per_relation: bool,
    damping: f64,
    tol: Option<f64>,
    max_iter: u32,
    seed: Option<(Dtype, Vec<f64>)>,
}

fn case() -> impl Strategy<Value = Case> {
    (1u64..10, 1usize..4).prop_flat_map(|(id_space, relation_count)| {
        let weight = prop::sample::select(vec![1.0, 0.5, 2.0, 3.0, 1e-3, 7.25, -1.0]);
        let edge = (0..relation_count, 0..id_space, 0..id_space, weight);
        let seed_value = prop::sample::select(vec![0.0, 1.0, 2.0, 0.25, 5.0, -1.0]);
        let seed = prop::option::of((
            prop::sample::select(vec![Dtype::F64, Dtype::F32, Dtype::I64]),
            prop::collection::vec(seed_value, id_space as usize),
        ));
        (
            prop::collection::vec(edge, 0..30),
            prop::collection::vec(0..id_space, 0..3),
            (any::<bool>(), any::<bool>()),
            prop::option::of(prop::collection::vec(0..relation_count, 0..3)),
            (prop::option::of(any::<bool>()), any::<bool>()),
            prop::sample::select(vec![0.85, 0.5, 1.0, 0.0, -0.5, 1.5]),
            prop::sample::select(vec![Some(1e-6), Some(1e-12), Some(0.0), None, Some(f64::NAN)]),
            0u32..40,
            seed,
        )
            .prop_map(
                move |(
                    edges,
                    tombstoned,
                    (build_reverse, store_merged),
                    selected,
                    (weighted, per_relation),
                    damping,
                    tol,
                    max_iter,
                    seed,
                )| Case {
                    id_space,
                    relation_count,
                    edges,
                    tombstoned,
                    layout: CsrLayout { build_reverse, store_merged },
                    selected,
                    weighted,
                    per_relation,
                    damping,
                    tol,
                    max_iter,
                    seed,
                },
            )
    })
}

fn seed_vector(id_space: u64, dtype: Dtype, values: &[f64]) -> AnyVector {
    let memory = memory();
    match dtype {
        Dtype::F64 => Vector::from_values(&memory, id_space, 1, values.to_vec()).unwrap().into(),
        Dtype::F32 => {
            let values = values.iter().map(|&value| value as f32).collect();
            Vector::from_values(&memory, id_space, 1, values).unwrap().into()
        }
        Dtype::I64 => {
            let values = values.iter().map(|&value| value as i64).collect();
            Vector::from_values(&memory, id_space, 1, values).unwrap().into()
        }
        Dtype::Bool => unreachable!("no bool seeds"),
    }
}

proptest! {
    /// The kernel computes what the PL/pgSQL library computed, to the bit,
    /// errors included, on random graphs.
    #[test]
    fn the_kernel_is_the_plpgsql_program_to_the_bit(case in case()) {
        let graph = reference::graph_as(case.id_space, case.relation_count, &case.edges, true, case.layout);
        let mut tombstones = Tombstones::new(case.id_space).unwrap();
        for &node in &case.tombstoned {
            tombstones.insert(node).unwrap();
        }
        let indexes = reference::relations(case.relation_count);
        let only: Option<Vec<RelationIndex>> =
            case.selected.as_ref().map(|selected| selected.iter().map(|&r| indexes[r]).collect());
        let seed = case.seed.as_ref().map(|(dtype, values)| seed_vector(case.id_space, *dtype, values));
        let pagerank = PageRank {
            relations: only.as_deref().map_or(RelationSelection::All, RelationSelection::Only),
            weighted: case.weighted,
            per_relation: case.per_relation,
            damping: case.damping,
            tol: case.tol,
            max_iter: case.max_iter,
            seed: seed.as_ref(),
        };
        let program = Program { graph: &graph, tombstones: &tombstones, pagerank: &pagerank, memory: memory() };
        let expected = program.run();
        let found = ranked(&graph, &tombstones, &pagerank);
        match (found, expected) {
            (Ok((found, i)), Ok((expected, j))) => {
                prop_assert_eq!(i, j);
                let bits = |values: &[f64]| values.iter().map(|value| value.to_bits()).collect::<Vec<_>>();
                prop_assert_eq!(bits(&found), bits(&expected), "{:?} against {:?}", found, expected);
            }
            (found, expected) => prop_assert_eq!(found.map(|_| ()), expected.map(|_| ())),
        }
    }
}
