use std::sync::OnceLock;

use proptest::prelude::*;

use super::*;
use crate::compute::reference::{self, Edges};
use crate::compute::{ComputeMemory, Workers};
use crate::interrupts::Uninterrupted;
use crate::spectral::reference as dense_reference;

fn workers() -> &'static Workers {
    static WORKERS: OnceLock<Workers> = OnceLock::new();
    WORKERS.get_or_init(|| Workers::new(3).unwrap())
}

/// Applies `matrix` over `graph` to `x`.
fn apply(graph: &GraphCsr, matrix: &GraphMatrix<'_>, x: &[f64]) -> Result<Vec<f64>, SpectralError> {
    let memory = ComputeMemory::new(1 << 24);
    let mut resources =
        Resources { workers: workers(), memory: &memory, interrupts: &mut Uninterrupted };
    let mut operator = LinearOperator::new(&mut resources, graph, matrix)?;
    operator.input_mut().copy_from_slice(x);
    Ok(operator.apply(&mut resources)?.to_vec())
}

/// The dense operator times `x`.
fn dense(
    id_space: u64,
    edges: &Edges,
    selected: Option<&[usize]>,
    weighted: bool,
    symmetrize: bool,
    operator: Operator,
    x: &[f64],
) -> Vec<f64> {
    let dense =
        dense_reference::operator(id_space, edges, selected, weighted, symmetrize, operator);
    (&dense.matrix * nalgebra::DVector::from_column_slice(x)).iter().copied().collect()
}

fn close(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| (a - b).abs() <= 1e-12 * (1.0 + a.abs()))
}

// Relation 0: 0 → 1 (2), 1 → 2 (4), 2 → 0 (1), 2 → 2 (3, a self-loop);
// relation 1: 0 → 1 (10), 3 → 0 (1). Node 4 is isolated.
fn example_edges() -> Edges {
    vec![
        (0, 0, 1, 2.0),
        (0, 1, 2, 4.0),
        (0, 2, 0, 1.0),
        (0, 2, 2, 3.0),
        (1, 0, 1, 10.0),
        (1, 3, 0, 1.0),
    ]
}

const X: [f64; 5] = [1.0, -2.0, 0.5, 3.0, 0.0];

#[test]
fn operators_parse_from_their_sql_names() {
    for operator in Operator::ALL {
        assert_eq!(operator.name().parse::<Operator>(), Ok(operator));
    }
    assert_eq!("svd".parse::<Operator>(), Err(SpectralError::UnknownOperator("svd".into())));
}

#[test]
fn each_operator_matches_its_dense_matrix() {
    let edges = example_edges();
    for reverse in [true, false] {
        let graph = reference::graph(5, 2, &edges, true, reverse);
        for operator in Operator::ALL {
            let got = apply(&graph, &GraphMatrix::new(operator), &X).unwrap();
            let want = dense(5, &edges, None, true, true, operator, &X);
            assert!(close(&got, &want), "{operator:?}: {got:?} != {want:?}");
        }
    }
}

#[test]
fn hand_computed_laplacian() {
    // S = (A + Aᵀ)/2 over the merged weights: 0–1: 12/2 = 6, 1–2: 2,
    // 0–2: 1/2, 2–2: 3, 0–3: 1/2. Degrees 7, 8, 5.5, 0.5, 0.
    let graph = reference::graph(5, 2, &example_edges(), true, true);
    let x = [1.0, 0.0, 0.0, 0.0, 0.0];
    let column = apply(&graph, &GraphMatrix::new(Operator::Laplacian), &x).unwrap();
    assert_eq!(column, [7.0, -6.0, -0.5, -0.5, 0.0]);
}

#[test]
fn isolated_and_tombstoned_nodes_are_inactive() {
    let graph = reference::graph(5, 2, &example_edges(), true, true);
    let memory = ComputeMemory::new(1 << 20);
    let mut resources =
        Resources { workers: workers(), memory: &memory, interrupts: &mut Uninterrupted };
    let operator = LinearOperator::new(
        &mut resources,
        &graph,
        &GraphMatrix::new(Operator::NormalizedLaplacian),
    )
    .unwrap();
    assert_eq!(operator.active, [true, true, true, true, false]);
    assert_eq!(operator.dimension(), 4);
    assert_eq!(operator.id_space(), 5);
}

#[test]
fn relations_and_weights_select_the_matrix() {
    let edges = example_edges();
    let graph = reference::graph(5, 2, &edges, true, true);
    let only = [reference::relations(2)[1]];
    let matrix = GraphMatrix {
        relations: RelationSelection::Only(&only),
        weighted: Some(false),
        ..GraphMatrix::new(Operator::Laplacian)
    };
    let got = apply(&graph, &matrix, &X).unwrap();
    assert!(close(&got, &dense(5, &edges, Some(&[1]), false, true, Operator::Laplacian, &X)));
}

#[test]
fn without_symmetrize_the_matrix_must_be_symmetric() {
    let directed = reference::graph(5, 2, &example_edges(), true, true);
    let matrix = GraphMatrix { symmetrize: false, ..GraphMatrix::new(Operator::Adjacency) };
    assert_eq!(apply(&directed, &matrix, &X), Err(SpectralError::NotSymmetric));
    let undirected_edges: Edges = example_edges()
        .into_iter()
        .flat_map(|(r, s, t, w)| [(r, s, t, w), (r, t, s, w)])
        .filter(|&(_, s, t, _)| s != t)
        .collect();
    for reverse in [true, false] {
        let undirected = reference::graph(5, 2, &undirected_edges, true, reverse);
        let symmetrized = apply(&undirected, &GraphMatrix::new(Operator::Laplacian), &X).unwrap();
        let laplacian = GraphMatrix { symmetrize: false, ..GraphMatrix::new(Operator::Laplacian) };
        assert_eq!(apply(&undirected, &laplacian, &X).unwrap(), symmetrized);
    }
}

/// Whether the operator over `graph` adds the transposed product.
fn transposes(graph: &GraphCsr, relations: RelationSelection<'_>) -> bool {
    let memory = ComputeMemory::new(1 << 24);
    let mut resources =
        Resources { workers: workers(), memory: &memory, interrupts: &mut Uninterrupted };
    let matrix = GraphMatrix { relations, ..GraphMatrix::new(Operator::NormalizedLaplacian) };
    LinearOperator::new(&mut resources, graph, &matrix).unwrap().transpose
}

#[test]
fn symmetrising_a_graph_whose_reverse_csrs_equal_its_forward_ones_multiplies_once() {
    // Each undirected relation's reverse CSR is its forward CSR, so A = Aᵀ
    // exactly and (A + Aᵀ)/2 is A: the transposed product would read the
    // same arrays in the same order and add nothing.
    let undirected: Edges = example_edges()
        .into_iter()
        .flat_map(|(r, s, t, w)| [(r, s, t, w), (r, t, s, w)])
        .filter(|&(_, s, t, _)| s != t)
        .collect();
    for store_merged in [true, false] {
        let layout = crate::graph::CsrLayout { build_reverse: true, store_merged };
        let graph = reference::graph_as(5, 2, &undirected, true, layout);
        assert!(!transposes(&graph, RelationSelection::All), "store_merged {store_merged}");
    }
    // Without reverse CSRs the check would cost a transpose: multiply twice.
    let unreversed = reference::graph(5, 2, &undirected, true, false);
    assert!(transposes(&unreversed, RelationSelection::All));
    let directed = reference::graph(5, 2, &example_edges(), true, true);
    assert!(transposes(&directed, RelationSelection::All));
    // Only the selected relations count: relation 1 alone of a graph whose
    // relation 0 is directed.
    let mixed: Edges = example_edges()
        .into_iter()
        .filter(|&(r, ..)| r == 0)
        .chain(undirected.iter().copied().filter(|&(r, ..)| r == 1))
        .collect();
    let graph = reference::graph(5, 2, &mixed, true, true);
    assert!(transposes(&graph, RelationSelection::All));
    let one = [reference::relations(2)[1]];
    assert!(!transposes(&graph, RelationSelection::Only(&one)));
}

#[test]
fn normalised_operators_need_positive_degrees() {
    let edges = vec![(0, 0, 1, 1.0), (0, 1, 2, -3.0)];
    let graph = reference::graph(3, 1, &edges, true, true);
    for operator in [Operator::NormalizedLaplacian, Operator::RandomWalk] {
        assert_eq!(
            apply(&graph, &GraphMatrix::new(operator), &[0.0; 3]),
            Err(SpectralError::NonPositiveDegree { node: 1, degree: -1.0 })
        );
    }
    assert!(apply(&graph, &GraphMatrix::new(Operator::Laplacian), &[0.0; 3]).is_ok());
}

#[test]
fn buffers_are_charged_to_the_budget() {
    let graph = reference::graph(5, 2, &example_edges(), true, true);
    let memory = ComputeMemory::new(5 * 8 * 4);
    let mut resources =
        Resources { workers: workers(), memory: &memory, interrupts: &mut Uninterrupted };
    let result =
        LinearOperator::new(&mut resources, &graph, &GraphMatrix::new(Operator::Adjacency));
    assert!(matches!(result, Err(SpectralError::Compute(ComputeError::MemoryLimit { .. }))));
}

fn scenarios() -> impl Strategy<Value = (u64, Edges, Vec<f64>, bool)> {
    (1u64..12).prop_flat_map(|n| {
        let edge = (0usize..2, 0..n, 0..n, 0.1f64..5.0);
        (
            Just(n),
            prop::collection::vec(edge, 0..30),
            prop::collection::vec(-1.0f64..1.0, n as usize),
            any::<bool>(),
        )
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn operators_match_their_dense_matrices((n, edges, x, reverse) in scenarios()) {
        let graph = reference::graph(n, 2, &edges, true, reverse);
        let dense_operator = dense_reference::operator(n, &edges, None, true, true, Operator::Adjacency);
        // Vectors the operator is applied to are zero at inactive nodes.
        let x: Vec<f64> = x.iter().zip(&dense_operator.active).map(|(&x, &a)| if a { x } else { 0.0 }).collect();
        for operator in Operator::ALL {
            let got = apply(&graph, &GraphMatrix::new(operator), &x).unwrap();
            let want = dense(n, &edges, None, true, true, operator, &x);
            prop_assert!(close(&got, &want), "{:?}: {:?} != {:?}", operator, got, want);
        }
    }
}
