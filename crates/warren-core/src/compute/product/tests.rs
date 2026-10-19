use std::sync::OnceLock;

use proptest::prelude::*;

use super::*;
use crate::compute::reference::{self, Edges, Write};
use crate::compute::{Dtype, Element, SemiringElement, Vector};
use crate::interrupts::{Cancelled, Uninterrupted};

fn workers() -> &'static Workers {
    static WORKERS: OnceLock<Workers> = OnceLock::new();
    WORKERS.get_or_init(|| Workers::new(3).unwrap())
}

fn memory() -> ComputeMemory {
    ComputeMemory::new(1 << 24)
}

fn vector<E: Element>(width: usize, values: Vec<E>) -> AnyVector
where
    AnyVector: From<Vector<E>>,
{
    let id_space = (values.len() / width) as u64;
    Vector::from_values(&memory(), id_space, width, values).unwrap().into()
}

fn values<E: Element>(vector: &AnyVector) -> Vec<E> {
    E::vector(vector).unwrap().values().to_vec()
}

// Relation 0: 0 → 1 (2), 0 → 2 (3), 1 → 2 (4), 2 → 0 (5); relation 1:
// 0 → 1 (10). Node 3 is isolated.
fn example(reverse: bool) -> GraphCsr {
    let edges =
        vec![(0, 0, 1, 2.0), (0, 0, 2, 3.0), (0, 1, 2, 4.0), (0, 2, 0, 5.0), (1, 0, 1, 10.0)];
    reference::graph(4, 2, &edges, true, reverse)
}

fn run(graph: &GraphCsr, product: &Product<'_>, x: &AnyVector, y: &mut AnyVector) {
    spmv(workers(), &memory(), &mut Uninterrupted, graph, product, x, y).unwrap();
}

fn plain(product: Product<'_>, reverse: bool) -> Vec<f64> {
    let x = vector(1, vec![1.0, 2.0, 3.0, 4.0]);
    let mut y = vector(1, vec![-1.0; 4]);
    run(&example(reverse), &product, &x, &mut y);
    values(&y)
}

#[test]
fn plus_times_multiplies_by_a_or_its_transpose() {
    let plus_times = Product::new(Semiring::PlusTimes);
    assert_eq!(plain(plus_times, true), [33.0, 12.0, 5.0, 0.0]);
    let transposed = Product { transpose: true, ..plus_times };
    assert_eq!(plain(transposed, true), [15.0, 12.0, 11.0, 0.0]);
    assert_eq!(plain(transposed, false), [15.0, 12.0, 11.0, 0.0]);
}

#[test]
fn relations_and_weights_select_the_matrix() {
    let r = reference::relations(2);
    let only = [r[1]];
    let product =
        Product { relations: RelationSelection::Only(&only), ..Product::new(Semiring::PlusTimes) };
    assert_eq!(plain(product, true), [20.0, 0.0, 0.0, 0.0]);
    let unweighted = Product { weighted: Some(false), ..Product::new(Semiring::PlusTimes) };
    assert_eq!(plain(unweighted, true), [5.0, 3.0, 1.0, 0.0]);
}

#[test]
fn an_empty_row_gets_the_add_identity() {
    let min_plus = Product::new("min_plus".parse().unwrap());
    assert_eq!(plain(min_plus, true), [6.0, 7.0, 6.0, f64::INFINITY]);
}

#[test]
fn masks_select_rows_and_replace_clears_the_rest() {
    let graph = example(true);
    let x = vector(1, vec![1.0, 2.0, 3.0, 4.0]);
    let mask = vector(1, vec![1i64, 0, -7, 0]);
    let write = |mask: Mask<'_>, replace: bool, accumulate: Option<BinaryOp>| {
        let mut y = vector(1, vec![100.0; 4]);
        let product =
            Product { mask: Some(mask), replace, accumulate, ..Product::new(Semiring::PlusTimes) };
        run(&graph, &product, &x, &mut y);
        values::<f64>(&y)
    };
    let selected = Mask { vector: &mask, complement: false };
    let complement = Mask { vector: &mask, complement: true };
    assert_eq!(write(selected, false, None), [33.0, 100.0, 5.0, 100.0]);
    assert_eq!(write(selected, true, None), [33.0, 0.0, 5.0, 0.0]);
    assert_eq!(write(selected, false, Some(BinaryOp::Plus)), [133.0, 100.0, 105.0, 100.0]);
    assert_eq!(write(complement, false, None), [100.0, 12.0, 100.0, 0.0]);
    assert_eq!(write(complement, true, Some(BinaryOp::Plus)), [0.0, 112.0, 0.0, 100.0]);
}

#[test]
fn spmm_combines_neighbour_rows_elementwise() {
    let graph = example(true);
    let x = vector(2, vec![1.0, 10.0, 2.0, 20.0, 3.0, 30.0, 4.0, 40.0]);
    let mut y = vector(2, vec![0.0; 8]);
    let product = Product { transpose: true, ..Product::new(Semiring::PlusTimes) };
    spmm(workers(), &memory(), &mut Uninterrupted, &graph, &product, &x, &mut y).unwrap();
    assert_eq!(values::<f64>(&y), [15.0, 150.0, 12.0, 120.0, 11.0, 110.0, 0.0, 0.0]);
}

fn error(
    graph: &GraphCsr,
    product: &Product<'_>,
    x: &AnyVector,
    y: &mut AnyVector,
    matrix: bool,
) -> ComputeError {
    let kernel = if matrix { spmm::<Uninterrupted> } else { spmv::<Uninterrupted> };
    kernel(workers(), &memory(), &mut Uninterrupted, graph, product, x, y).unwrap_err()
}

#[test]
fn operands_must_fit_the_graph_and_each_other() {
    let graph = example(true);
    let product = Product::new(Semiring::PlusTimes);
    let x = vector(1, vec![1.0; 4]);
    assert_eq!(
        error(&graph, &product, &x, &mut vector(1, vec![0i64; 4]), false),
        ComputeError::DtypeMismatch { expected: Dtype::I64, found: Dtype::F64 }
    );
    assert_eq!(
        error(&graph, &product, &vector(1, vec![1.0; 3]), &mut vector(1, vec![0.0; 3]), false),
        ComputeError::IdSpaceMismatch { expected: 4, found: 3 }
    );
    assert_eq!(
        error(&graph, &product, &vector(2, vec![1.0; 8]), &mut vector(2, vec![0.0; 8]), false),
        ComputeError::WidthMismatch { expected: 1, found: 2 }
    );
    assert_eq!(
        error(&graph, &product, &vector(2, vec![1.0; 8]), &mut vector(1, vec![0.0; 4]), true),
        ComputeError::WidthMismatch { expected: 1, found: 2 }
    );
    let mask = vector(2, vec![1.0; 8]);
    let masked = Product { mask: Some(Mask { vector: &mask, complement: false }), ..product };
    assert_eq!(
        error(&graph, &masked, &x, &mut vector(1, vec![0.0; 4]), false),
        ComputeError::WidthMismatch { expected: 1, found: 2 }
    );
    let short_mask = vector(1, vec![1.0; 3]);
    let masked = Product { mask: Some(Mask { vector: &short_mask, complement: false }), ..product };
    assert_eq!(
        error(&graph, &masked, &x, &mut vector(1, vec![0.0; 4]), false),
        ComputeError::IdSpaceMismatch { expected: 4, found: 3 }
    );
    let unknown = [reference::relations(3)[2]];
    let product = Product { relations: RelationSelection::Only(&unknown), ..product };
    assert_eq!(
        error(&graph, &product, &x, &mut vector(1, vec![0.0; 4]), false),
        ComputeError::UnknownRelation { relation: 2 }
    );
}

#[test]
fn i64_min_plus_supersteps_keep_unreached_nodes_at_the_identity() {
    // 0 → 1 (1), 2 → 1 (1): node 2 is never reached from node 0.
    let graph = reference::graph(3, 1, &vec![(0, 0, 1, 1.0), (0, 2, 1, 1.0)], true, true);
    let product = Product {
        transpose: true,
        accumulate: Some(BinaryOp::Min),
        ..Product::new("min_plus".parse().unwrap())
    };
    let mut distance = vector(1, vec![0i64, i64::MAX, i64::MAX]);
    for _ in 0..2 {
        let x = vector(1, values::<i64>(&distance));
        run(&graph, &product, &x, &mut distance);
        assert_eq!(values::<i64>(&distance), [0, 1, i64::MAX]);
    }
    let longest =
        Product { accumulate: Some(BinaryOp::Max), ..Product::new("max_plus".parse().unwrap()) };
    let mut y = vector(1, vec![i64::MIN; 3]);
    run(
        &graph,
        &Product { transpose: true, ..longest },
        &vector(1, vec![5, i64::MIN, i64::MIN]),
        &mut y,
    );
    assert_eq!(values::<i64>(&y), [i64::MIN, 6, i64::MIN]);
}

#[test]
fn i64_overflow_is_an_error() {
    let graph = example(true);
    let x = vector(1, vec![i64::MAX; 4]);
    let mut y = vector(1, vec![0i64; 4]);
    let product = Product::new(Semiring::PlusTimes);
    assert_eq!(
        error(&graph, &product, &x, &mut y, false),
        ComputeError::Arithmetic(crate::compute::ArithmeticError::BigintOutOfRange)
    );
}

#[test]
fn an_interrupt_cancels_the_product() {
    struct Stop;
    impl Interrupts for Stop {
        fn check(&mut self) -> Result<(), Cancelled> {
            Err(Cancelled)
        }
    }
    let graph = example(false);
    let x = vector(1, vec![1.0; 4]);
    for transpose in [false, true] {
        let product = Product { transpose, ..Product::new(Semiring::PlusTimes) };
        let mut y = vector(1, vec![0.0; 4]);
        let result = spmv(workers(), &memory(), &mut Stop, &graph, &product, &x, &mut y);
        assert_eq!(result, Err(ComputeError::Cancelled));
    }
}

#[test]
fn the_push_form_charges_its_scratch_to_the_budget() {
    let graph = example(false);
    let x = vector(1, vec![1.0; 4]);
    let mut y = vector(1, vec![0.0; 4]);
    let product = Product { transpose: true, ..Product::new(Semiring::PlusTimes) };
    let tight = ComputeMemory::new(8);
    let result = spmv(workers(), &tight, &mut Uninterrupted, &graph, &product, &x, &mut y);
    assert!(matches!(result, Err(ComputeError::MemoryLimit { .. })));
    assert_eq!(tight.in_use(), 0);
}

/// A random product over a random graph.
#[derive(Debug, Clone)]
struct Scenario {
    id_space: u64,
    relation_count: usize,
    edges: Edges,
    weighted_graph: bool,
    reverse: bool,
    semiring: Semiring,
    transpose: bool,
    selected: Option<Vec<usize>>,
    weighted: Option<bool>,
    mask: Option<(Vec<f64>, bool)>,
    replace: bool,
    accumulate: Option<BinaryOp>,
    width: usize,
}

fn binary_ops() -> impl Strategy<Value = BinaryOp> {
    prop::sample::select(
        ["first", "second", "pair", "plus", "times", "min", "max", "lor", "land", "any"]
            .map(|name| name.parse().unwrap())
            .to_vec(),
    )
}

fn scenarios(max_width: usize) -> impl Strategy<Value = Scenario> {
    (1u64..14, 1usize..4, any::<bool>(), 1..=max_width).prop_flat_map(
        move |(id_space, relation_count, weighted_graph, width)| {
            let weight = if weighted_graph {
                (0u8..4).prop_map(f64::from).boxed()
            } else {
                Just(1.0).boxed()
            };
            let edge = (0..relation_count, 0..id_space, 0..id_space, weight);
            let mask_value = prop::sample::select(vec![0.0, -0.0, 1.0, 2.5, f64::NAN]);
            (
                prop::collection::vec(edge, 0..50),
                any::<bool>(),
                prop::sample::select(Semiring::all().collect::<Vec<_>>()),
                any::<bool>(),
                prop::option::of(prop::collection::vec(0..relation_count, 0..3)),
                prop::option::of(any::<bool>()),
                prop::option::of((
                    prop::collection::vec(mask_value, id_space as usize),
                    any::<bool>(),
                )),
                any::<bool>(),
                prop::option::of(binary_ops()),
            )
                .prop_map(
                    move |(
                        edges,
                        reverse,
                        semiring,
                        transpose,
                        selected,
                        weighted,
                        mask,
                        replace,
                        accumulate,
                    )| {
                        Scenario {
                            id_space,
                            relation_count,
                            edges,
                            weighted_graph,
                            reverse,
                            semiring,
                            transpose,
                            selected,
                            weighted,
                            mask,
                            replace,
                            accumulate,
                            width,
                        }
                    },
                )
        },
    )
}

/// Runs the scenario through the kernel and the dense reference.
fn check<E: SemiringElement>(scenario: &Scenario, x: Vec<E>, y: Vec<E>) -> Result<(), TestCaseError>
where
    AnyVector: From<Vector<E>>,
{
    let s = scenario;
    let graph =
        reference::graph(s.id_space, s.relation_count, &s.edges, s.weighted_graph, s.reverse);
    let indexes = reference::relations(s.relation_count);
    let selected: Option<Vec<_>> =
        s.selected.as_ref().map(|r| r.iter().map(|&r| indexes[r]).collect());
    let mask = s.mask.as_ref().map(|(values, _)| vector::<f64>(1, values.clone()));
    let product = Product {
        semiring: s.semiring,
        transpose: s.transpose,
        relations: selected.as_deref().map_or(RelationSelection::All, RelationSelection::Only),
        weighted: s.weighted,
        mask: mask
            .as_ref()
            .zip(s.mask.as_ref())
            .map(|(vector, &(_, complement))| Mask { vector, complement }),
        replace: s.replace,
        accumulate: s.accumulate,
    };
    let weighted = s.weighted.unwrap_or(true) && s.weighted_graph;
    let a = reference::adjacency(s.id_space, &s.edges, s.selected.as_deref(), weighted);
    let write = Write {
        selected: match &s.mask {
            None => vec![true; s.id_space as usize],
            Some((values, complement)) => {
                values.iter().map(|v| v.is_nonzero() != *complement).collect()
            }
        },
        replace: s.replace,
        accumulate: s.accumulate,
    };
    let expected = reference::product(&a, s.transpose, s.semiring, s.width, &x, &y, &write);

    let x = vector(s.width, x);
    let mut y = vector(s.width, y);
    let kernel = if s.width == 1 { spmv::<Uninterrupted> } else { spmm::<Uninterrupted> };
    let found = kernel(workers(), &memory(), &mut Uninterrupted, &graph, &product, &x, &mut y);
    match expected {
        Ok(expected) => {
            prop_assert_eq!(found, Ok(()));
            let found = values::<E>(&y);
            prop_assert!(
                found.iter().zip(&expected).all(|(a, b)| a.sql_cmp(*b).is_eq()),
                "found {:?}, expected {:?}",
                found,
                expected
            );
        }
        Err(error) => prop_assert_eq!(found, Err(ComputeError::Arithmetic(error))),
    }
    Ok(())
}

fn f64s(len: usize) -> impl Strategy<Value = Vec<f64>> {
    let value = prop_oneof![
        8 => (-4i8..5).prop_map(f64::from),
        1 => Just(f64::NAN),
        1 => Just(f64::INFINITY),
        1 => Just(0.5),
    ];
    prop::collection::vec(value, len)
}

fn i64s(len: usize) -> impl Strategy<Value = Vec<i64>> {
    let value = prop_oneof![8 => -4i64..5, 1 => Just(i64::MAX), 1 => Just(i64::MIN)];
    prop::collection::vec(value, len)
}

fn with_values<T: std::fmt::Debug + Clone>(
    max_width: usize,
    values: fn(usize) -> BoxedStrategy<Vec<T>>,
) -> impl Strategy<Value = (Scenario, Vec<T>, Vec<T>)> {
    scenarios(max_width).prop_flat_map(move |scenario| {
        let len = scenario.id_space as usize * scenario.width;
        (Just(scenario), values(len), values(len))
    })
}

proptest! {
    #[test]
    fn f64_products_match_the_dense_reference(
        (scenario, x, y) in with_values(1, |len| f64s(len).boxed())
    ) {
        check(&scenario, x, y)?;
    }

    #[test]
    fn f32_products_match_the_dense_reference(
        (scenario, x, y) in with_values(1, |len| f64s(len).prop_map(|v| v.into_iter().map(|v| v as f32).collect()).boxed())
    ) {
        check::<f32>(&scenario, x, y)?;
    }

    #[test]
    fn i64_products_match_the_dense_reference(
        (scenario, x, y) in with_values(1, |len| i64s(len).boxed())
    ) {
        check(&scenario, x, y)?;
    }

    #[test]
    fn bool_products_match_the_dense_reference(
        (scenario, x, y) in with_values(1, |len| prop::collection::vec(any::<bool>(), len).boxed())
    ) {
        check(&scenario, x, y)?;
    }

    #[test]
    fn matrix_products_match_the_dense_reference(
        (scenario, x, y) in with_values(4, |len| f64s(len).boxed())
    ) {
        check(&scenario, x, y)?;
    }

    #[test]
    fn pushing_agrees_with_pulling_bit_for_bit(
        (scenario, x, y) in with_values(3, |len| f64s(len).prop_map(|v| v.into_iter().map(|v| v * 0.1).collect()).boxed())
    ) {
        let s = Scenario { transpose: true, ..scenario };
        let mut results = Vec::new();
        for reverse in [false, true] {
            let graph = reference::graph(s.id_space, s.relation_count, &s.edges, s.weighted_graph, reverse);
            let x = vector(s.width, x.clone());
            let mut y = vector(s.width, y.clone());
            let product = Product { transpose: true, semiring: s.semiring, ..Product::new(s.semiring) };
            let result = spmm(workers(), &memory(), &mut Uninterrupted, &graph, &product, &x, &mut y);
            results.push(result.map(|()| values::<f64>(&y).iter().map(|v| v.to_bits()).collect::<Vec<_>>()));
        }
        prop_assert_eq!(&results[0], &results[1]);
    }
}

#[test]
fn rows_span_many_tasks() {
    // More rows than one task holds, so tasks split the rows.
    let id_space: u64 = 4099;
    let edges: Edges = (0..id_space).map(|i| (0, i, (i * 7 + 1) % id_space, 1.0)).collect();
    let graph = reference::graph(id_space, 1, &edges, false, true);
    let x: Vec<f64> = (0..id_space).map(|i| i as f64).collect();
    for transpose in [false, true] {
        let mut y = vector(1, vec![0.0; id_space as usize]);
        let product = Product { transpose, ..Product::new(Semiring::PlusTimes) };
        run(&graph, &product, &vector(1, x.clone()), &mut y);
        let expected: Vec<f64> = (0..id_space)
            .map(|i| match transpose {
                false => ((i * 7 + 1) % id_space) as f64,
                true => (0..id_space).find(|j| (j * 7 + 1) % id_space == i).unwrap() as f64,
            })
            .collect();
        assert_eq!(values::<f64>(&y), expected);
    }
}
