use std::f64::consts::PI;
use std::sync::OnceLock;

use nalgebra::{DMatrix, DVector};
use proptest::prelude::*;

use super::*;
use crate::compute::reference::{self, Edges};
use crate::compute::{ComputeError, ComputeMemory, Element, Workers};
use crate::interrupts::{Cancelled, Uninterrupted};
use crate::spectral::reference::{self as dense_reference, Dense};
use crate::spectral::Operator;

fn workers(threads: usize) -> &'static Workers {
    static POOLS: OnceLock<Vec<Workers>> = OnceLock::new();
    &POOLS.get_or_init(|| (1..=4).map(|t| Workers::new(t).unwrap()).collect())[threads - 1]
}

fn solve_on(
    threads: usize,
    graph: &GraphCsr,
    matrix: &GraphMatrix<'_>,
    options: &EigenOptions,
) -> Result<Eigen, SpectralError> {
    let memory = ComputeMemory::new(1 << 26);
    let mut resources =
        Resources { workers: workers(threads), memory: &memory, interrupts: &mut Uninterrupted };
    eigen(&mut resources, graph, matrix, options)
}

fn solve(graph: &GraphCsr, operator: Operator, options: &EigenOptions) -> Eigen {
    solve_on(3, graph, &GraphMatrix::new(operator), options).unwrap()
}

fn as_f64(vector: &AnyVector) -> Vec<f64> {
    match vector {
        AnyVector::F32(v) => v.values().iter().map(|&x| x.to_f64()).collect(),
        AnyVector::F64(v) => v.values().to_vec(),
        _ => panic!("eigenvectors are floats"),
    }
}

/// An undirected path `0 – 1 – … – n−1` with unit weights.
fn path(n: u64) -> Edges {
    (0..n - 1).flat_map(|i| [(0, i, i + 1, 1.0), (0, i + 1, i, 1.0)]).collect()
}

/// The `k` reference eigenvalues at the `which` end, in `which` order.
fn wanted(pairs: &[(f64, DVector<f64>)], k: usize, which: Which) -> Vec<(f64, DVector<f64>)> {
    let mut pairs = pairs.to_vec();
    if which == Which::Largest {
        pairs.sort_by(|a, b| b.0.abs().total_cmp(&a.0.abs()).then(b.0.total_cmp(&a.0)));
    }
    pairs.truncate(k);
    pairs
}

#[test]
fn which_parses_from_its_sql_name() {
    assert_eq!("largest".parse(), Ok(Which::Largest));
    assert_eq!("smallest".parse(), Ok(Which::Smallest));
    assert_eq!("middle".parse::<Which>(), Err(SpectralError::UnknownWhich("middle".into())));
}

#[test]
fn the_smallest_laplacian_eigenpairs_of_a_path() {
    // L of the path P_n has eigenvalues 2 − 2cos(πj/n), j = 0..n−1, with
    // eigenvectors cos(πj(i + ½)/n).
    let n = 60;
    let graph = reference::graph(n, 1, &path(n), false, true);
    let options =
        EigenOptions { which: Which::Smallest, precision: Precision::F64, ..EigenOptions::new(4) };
    let eigen = solve(&graph, Operator::Laplacian, &options);
    assert!(eigen.converged);
    assert_eq!(eigen.pairs.len(), 4);
    for (j, pair) in eigen.pairs.iter().enumerate() {
        let expected = 2.0 - 2.0 * (PI * j as f64 / n as f64).cos();
        assert!((pair.value - expected).abs() < 1e-9, "λ{j} = {} != {expected}", pair.value);
        assert!(pair.residual < 1e-7, "residual {}", pair.residual);
        let mut exact: Vec<f64> =
            (0..n).map(|i| (PI * j as f64 * (i as f64 + 0.5) / n as f64).cos()).collect();
        let norm = exact.iter().map(|x| x * x).sum::<f64>().sqrt();
        exact.iter_mut().for_each(|x| *x /= norm);
        let dot: f64 = as_f64(&pair.vector).iter().zip(&exact).map(|(a, b)| a * b).sum();
        assert!((dot.abs() - 1.0).abs() < 1e-8, "eigenvector {j}: |⟨x, exact⟩| = {}", dot.abs());
    }
}

#[test]
fn eigenvectors_have_a_fixed_sign() {
    // The entry of largest magnitude is positive.
    let graph = reference::graph(30, 2, &random_graph(30, 60, 1), true, true);
    let eigen = solve(&graph, Operator::Adjacency, &EigenOptions::new(3));
    for pair in &eigen.pairs {
        let x = as_f64(&pair.vector);
        let top = x.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let first = x.iter().find(|v| v.abs() == top).unwrap();
        assert!(*first > 0.0);
    }
}

#[test]
fn inactive_nodes_contribute_nothing() {
    // Path 0 – 1 – 2 – 3 – 4 plus node 5 isolated and node 6 tombstoned
    // (no edges): one zero Laplacian eigenvalue, not three.
    let graph = reference::graph(7, 1, &path(5), false, true);
    let options = EigenOptions { which: Which::Smallest, ..EigenOptions::new(5) };
    let eigen = solve(&graph, Operator::Laplacian, &options);
    assert!(eigen.pairs[0].value.abs() < 1e-6);
    assert!(eigen.pairs[1].value > 0.3);
    for pair in &eigen.pairs {
        assert_eq!(&as_f64(&pair.vector)[5..], [0.0, 0.0]);
    }
    let too_many =
        solve_on(1, &graph, &GraphMatrix::new(Operator::Laplacian), &EigenOptions::new(6));
    assert_eq!(too_many.unwrap_err(), SpectralError::EigenpairCount { k: 6, dimension: 5 });
    let none = solve_on(1, &graph, &GraphMatrix::new(Operator::Laplacian), &EigenOptions::new(0));
    assert_eq!(none.unwrap_err(), SpectralError::EigenpairCount { k: 0, dimension: 5 });
}

/// A connected random graph: a path with random weights plus random
/// directed edges, so the symmetrised spectrum is generically simple.
fn random_graph(n: u64, extra: usize, seed: u64) -> Edges {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let weight = |next: &mut dyn FnMut() -> u64| 0.5 + (next() % 1000) as f64 / 400.0;
    let mut edges: Edges = (0..n - 1).map(|i| (0, i, i + 1, weight(&mut next))).collect();
    for _ in 0..extra {
        let (s, t) = (next() % n, next() % n);
        edges.push(((next() % 2) as usize, s, t, weight(&mut next)));
    }
    edges
}

/// The dense random-walk matrix `D^-1 S` over the same edges.
fn random_walk(n: u64, edges: &Edges) -> DMatrix<f64> {
    let s = dense_reference::operator(n, edges, None, true, true, Operator::Adjacency).matrix;
    let degrees: Vec<f64> = (0..n as usize).map(|i| s.row(i).sum()).collect();
    DMatrix::from_fn(n as usize, n as usize, |i, j| s[(i, j)] / degrees[i])
}

/// Checks `eigen` against the dense operator: eigenvalues within their
/// residuals of the reference's, unit orthogonal eigenvectors that satisfy
/// the dense eigen-equation.
fn check(
    n: u64,
    edges: &Edges,
    operator: Operator,
    options: &EigenOptions,
    eigen: &Eigen,
) -> Result<(), TestCaseError> {
    let dense: Dense = dense_reference::operator(n, edges, None, true, true, operator);
    let reference = dense.eigenpairs();
    let scale = reference.iter().fold(0.0f64, |m, p| m.max(p.0.abs()));
    let accuracy = match options.precision {
        Precision::F64 => 1e-7,
        Precision::F32 => 1e-4,
    } * scale.max(1.0);
    let wanted = wanted(&reference, options.k, options.which);
    prop_assert!(eigen.converged);
    prop_assert_eq!(eigen.pairs.len(), options.k);
    let matrix = match operator {
        Operator::RandomWalk => random_walk(n, edges),
        _ => dense.matrix.clone(),
    };
    let vectors: Vec<DVector<f64>> =
        eigen.pairs.iter().map(|p| DVector::from_vec(as_f64(&p.vector))).collect();
    for (i, (pair, x)) in eigen.pairs.iter().zip(&vectors).enumerate() {
        // By magnitude, λ and −λ tie (a bipartite graph's adjacency has a
        // symmetric spectrum), so compare magnitudes, then check that the
        // value is in the spectrum.
        let (got, want) = match options.which {
            Which::Largest => (pair.value.abs(), wanted[i].0.abs()),
            Which::Smallest => (pair.value, wanted[i].0),
        };
        prop_assert!((got - want).abs() <= accuracy, "λ{} = {} != {}", i, pair.value, wanted[i].0);
        prop_assert!(reference.iter().any(|(value, _)| (value - pair.value).abs() <= accuracy));
        prop_assert!(pair.residual <= accuracy, "residual {} > {}", pair.residual, accuracy);
        prop_assert!((x.norm() - 1.0).abs() <= accuracy);
        let residual = (&matrix * x - x * pair.value).norm();
        prop_assert!(residual <= accuracy, "dense residual {} of pair {}", residual, i);
        if operator != Operator::RandomWalk {
            for y in &vectors[..i] {
                prop_assert!(x.dot(y).abs() <= accuracy.sqrt());
            }
        }
    }
    Ok(())
}

fn operators() -> impl Strategy<Value = Operator> {
    prop::sample::select(Operator::ALL.to_vec())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn lanczos_matches_a_dense_eigensolver(
        n in 8u64..120,
        density in 0usize..4,
        seed in any::<u64>(),
        k in 1usize..6,
        operator in operators(),
        smallest in any::<bool>(),
        f64_vectors in any::<bool>(),
    ) {
        let edges = random_graph(n, density * n as usize, seed);
        let graph = reference::graph(n, 2, &edges, true, seed % 2 == 0);
        let options = EigenOptions {
            which: if smallest { Which::Smallest } else { Which::Largest },
            precision: if f64_vectors { Precision::F64 } else { Precision::F32 },
            seed,
            ..EigenOptions::new(k)
        };
        let eigen = solve(&graph, operator, &options);
        check(n, &edges, operator, &options, &eigen)?;
    }
}

#[test]
fn a_large_graph_restarts_and_still_converges() {
    let n = 200;
    let edges = random_graph(n, 600, 7);
    let graph = reference::graph(n, 2, &edges, true, true);
    for precision in [Precision::F64, Precision::F32] {
        let options = EigenOptions { which: Which::Smallest, precision, ..EigenOptions::new(3) };
        let eigen = solve(&graph, Operator::NormalizedLaplacian, &options);
        assert!(eigen.iterations > 20, "{} steps: no restart exercised", eigen.iterations);
        check(n, &edges, Operator::NormalizedLaplacian, &options, &eigen).unwrap();
    }
}

#[test]
fn results_are_identical_for_every_thread_count() {
    let n = 150;
    let edges = random_graph(n, 450, 11);
    let graph = reference::graph(n, 2, &edges, true, false);
    for precision in [Precision::F32, Precision::F64] {
        let options =
            EigenOptions { which: Which::Smallest, precision, seed: 5, ..EigenOptions::new(4) };
        let matrix = GraphMatrix::new(Operator::NormalizedLaplacian);
        let runs: Vec<Eigen> =
            (1..=4).map(|t| solve_on(t, &graph, &matrix, &options).unwrap()).collect();
        for run in &runs[1..] {
            assert_eq!(run, &runs[0]);
        }
        assert_eq!(solve_on(2, &graph, &matrix, &options).unwrap(), runs[0]);
    }
}

#[test]
fn the_seed_changes_the_iterates_not_the_eigenvalues() {
    let edges = random_graph(80, 200, 3);
    let graph = reference::graph(80, 2, &edges, true, true);
    let run = |seed| {
        solve(
            &graph,
            Operator::Laplacian,
            &EigenOptions { seed, precision: Precision::F64, ..EigenOptions::new(3) },
        )
    };
    let (a, b) = (run(1), run(2));
    for (a, b) in a.pairs.iter().zip(&b.pairs) {
        assert!((a.value - b.value).abs() < 1e-7 * a.value.abs());
    }
}

#[test]
fn max_iter_stops_early_and_reports_residuals() {
    let edges = random_graph(200, 600, 9);
    let graph = reference::graph(200, 2, &edges, true, true);
    let options = EigenOptions {
        which: Which::Smallest,
        max_iter: Some(25),
        tol: 1e-12,
        ..EigenOptions::new(3)
    };
    let eigen = solve(&graph, Operator::NormalizedLaplacian, &options);
    assert!(!eigen.converged);
    assert_eq!(eigen.iterations, 25);
    assert_eq!(eigen.pairs.len(), 3);
    assert!(eigen.pairs.iter().all(|p| p.residual > 0.0 && p.residual.is_finite()));
}

#[test]
fn the_lanczos_vectors_are_charged_to_the_budget() {
    let n = 500;
    let graph = reference::graph(n, 1, &path(n), false, true);
    // The operator's buffers fit, the basis of 20 f64 vectors does not.
    let memory = ComputeMemory::new(n * 8 * 12);
    let mut resources =
        Resources { workers: workers(1), memory: &memory, interrupts: &mut Uninterrupted };
    let options = EigenOptions { precision: Precision::F64, ..EigenOptions::new(2) };
    let result = eigen(&mut resources, &graph, &GraphMatrix::new(Operator::Adjacency), &options);
    assert!(matches!(result, Err(SpectralError::Compute(ComputeError::MemoryLimit { .. }))));
    assert_eq!(memory.in_use(), 0);
}

/// Asks to stop on the `n`th poll.
struct StopAfter(usize);

impl Interrupts for StopAfter {
    fn check(&mut self) -> Result<(), Cancelled> {
        self.0 = self.0.saturating_sub(1);
        match self.0 {
            0 => Err(Cancelled),
            _ => Ok(()),
        }
    }
}

#[test]
fn an_interrupt_between_steps_cancels() {
    let graph = reference::graph(100, 1, &path(100), false, true);
    let memory = ComputeMemory::new(1 << 24);
    let mut interrupts = StopAfter(200);
    let mut resources =
        Resources { workers: workers(2), memory: &memory, interrupts: &mut interrupts };
    let options = EigenOptions {
        which: Which::Smallest,
        tol: 0.0,
        max_iter: Some(10_000),
        ..EigenOptions::new(2)
    };
    let result = eigen(&mut resources, &graph, &GraphMatrix::new(Operator::Laplacian), &options);
    assert_eq!(result.unwrap_err(), SpectralError::Compute(ComputeError::Cancelled));
}

#[test]
fn eigenvectors_have_the_precision_s_element_type() {
    let graph = reference::graph(10, 1, &path(10), false, true);
    for (precision, dtype) in [(Precision::F32, f32::DTYPE), (Precision::F64, f64::DTYPE)] {
        let eigen =
            solve(&graph, Operator::Adjacency, &EigenOptions { precision, ..EigenOptions::new(2) });
        assert!(eigen.pairs.iter().all(|p| p.vector.dtype() == dtype && p.vector.id_space() == 10));
    }
}
