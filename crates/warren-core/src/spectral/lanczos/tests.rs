use std::f64::consts::PI;
use std::sync::OnceLock;

use nalgebra::{DMatrix, DVector};
use proptest::prelude::*;

use super::*;
use crate::compute::reference::{self, Edges};
use crate::compute::{ComputeError, ComputeMemory, Element, Workers};
use crate::graph::RelationSelection;
use crate::interrupts::{Cancelled, Uninterrupted};
use crate::progress::recorded::Recorded;
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
fn precision_parses_from_its_dtype_name() {
    assert_eq!("f32".parse(), Ok(Precision::F32));
    assert_eq!("f64".parse(), Ok(Precision::F64));
    assert_eq!("i64".parse::<Precision>(), Err(SpectralError::UnknownPrecision("i64".into())));
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
    let options = EigenOptions {
        which: Which::Smallest,
        precision: Precision::F64,
        tol: 1e-8,
        ..EigenOptions::new(4)
    };
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

#[test]
fn a_solve_is_admitted_or_refused_without_solving() {
    let graph = reference::graph(200, 1, &path(200), false, true);
    let options = EigenOptions::new(4);
    let matrix = GraphMatrix::new(Operator::Laplacian);
    let needed = path_working_set(200, Operator::Laplacian, 4, Precision::F32).bytes();
    for (limit, admitted) in [(needed - 1, false), (needed, true)] {
        let memory = ComputeMemory::new(limit);
        let mut resources =
            Resources { workers: workers(1), memory: &memory, interrupts: &mut Uninterrupted };
        let found = admit_eigen(&mut resources, &graph, &matrix, &options);
        assert_eq!(found.is_ok(), admitted, "{found:?}");
        assert_eq!(found.err(), solve_within(limit, &graph, &matrix, &options).err());
        assert_eq!(memory.in_use(), 0);
    }
    let too_many = EigenOptions::new(201);
    let memory = ComputeMemory::new(1 << 24);
    let mut resources =
        Resources { workers: workers(1), memory: &memory, interrupts: &mut Uninterrupted };
    let refused = admit_eigen(&mut resources, &graph, &matrix, &too_many).unwrap_err();
    assert_eq!(refused, SpectralError::EigenpairCount { k: 201, dimension: 200 });
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
            // The checks' f64 accuracy of 1e-7 needs a tighter tolerance than
            // the default.
            tol: 1e-8,
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
        // The check's f64 accuracy of 1e-7 needs a tighter tolerance than
        // the default.
        let options =
            EigenOptions { which: Which::Smallest, precision, tol: 1e-8, ..EigenOptions::new(3) };
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
    assert!(matches!(result, Err(SpectralError::WorkingSet { .. })), "{result:?}");
    assert_eq!(memory.in_use(), 0);
}

/// The working set of the `k` largest eigenpairs of `operator` over
/// [`path`]`(n)`, every node of which is active, in one bipartite
/// component: the random walk's eigenvalues 1 and −1 and the normalised
/// Laplacian's 2 are the component's, and Lanczos computes the rest.
fn path_working_set(n: u64, operator: Operator, k: usize, precision: Precision) -> WorkingSet {
    let vectors = match operator {
        Operator::RandomWalk => 2,
        Operator::NormalizedLaplacian => 1,
        _ => 0,
    };
    let deflated = (vectors > 0).then_some(Deflated { components: 1, vectors });
    let wanted = k.saturating_sub(vectors as usize);
    WorkingSet { id_space: n, dimension: n, operator, k, precision, deflated, wanted }
}

#[test]
fn the_operator_charges_what_its_working_set_counts() {
    let graph = reference::graph(300, 1, &path(300), false, true);
    for operator in Operator::ALL {
        let memory = ComputeMemory::new(1 << 24);
        let mut resources =
            Resources { workers: workers(1), memory: &memory, interrupts: &mut Uninterrupted };
        let _operator =
            LinearOperator::new(&mut resources, &graph, &GraphMatrix::new(operator)).unwrap();
        let expected = path_working_set(300, operator, 1, Precision::F32).operator_bytes();
        assert_eq!(memory.in_use(), expected, "{operator:?}");
    }
}

/// Solves `matrix` over `graph` within a budget of `limit` bytes, and
/// returns the steps it took, checking that nothing stays charged.
fn solve_within(
    limit: u64,
    graph: &GraphCsr,
    matrix: &GraphMatrix<'_>,
    options: &EigenOptions,
) -> Result<usize, SpectralError> {
    let memory = ComputeMemory::new(limit);
    let mut resources =
        Resources { workers: workers(2), memory: &memory, interrupts: &mut Uninterrupted };
    let steps = eigen(&mut resources, graph, matrix, options).map(|eigen| eigen.iterations);
    assert_eq!(memory.in_use(), 0);
    steps
}

#[test]
fn a_solve_that_cannot_fit_is_refused_before_its_first_step() {
    // The basis and the operator fit; the solve would run its 2,000 steps
    // and only then fail to allocate its eigenvectors.
    let graph = reference::graph(500, 1, &path(500), false, true);
    let options = EigenOptions { tol: 0.0, max_iter: Some(2000), ..EigenOptions::new(10) };
    let set = path_working_set(500, Operator::Adjacency, 10, Precision::F32);
    // 500 rows in blocks of 7 make 504, and the basis holds 21 vectors.
    let limit = set.operator_bytes() + 504 * 21 * 4 + 1000;
    let refused = solve_within(limit, &graph, &GraphMatrix::new(Operator::Adjacency), &options);
    let needed = set.bytes();
    assert_eq!(refused.unwrap_err(), SpectralError::WorkingSet { needed, in_use: 0, limit });
}

#[test]
fn a_budget_one_byte_short_of_the_working_set_is_refused() {
    let graph = reference::graph(200, 1, &path(200), false, true);
    let options = EigenOptions { precision: Precision::F64, ..EigenOptions::new(3) };
    let matrix = GraphMatrix::new(Operator::RandomWalk);
    let needed = path_working_set(200, Operator::RandomWalk, 3, Precision::F64).bytes();
    let refused = solve_within(needed - 1, &graph, &matrix, &options).unwrap_err();
    assert_eq!(refused, SpectralError::WorkingSet { needed, in_use: 0, limit: needed - 1 });
    assert!(solve_within(needed, &graph, &matrix, &options).is_ok_and(|steps| steps > 0));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn a_solve_fits_a_budget_of_its_working_set(
        operator in operators(),
        k in 1usize..14,
        f64s in any::<bool>(),
        reverse in any::<bool>(),
        n in 30u64..120,
    ) {
        // Without reverse CSRs the symmetrised product pushes, with a
        // scratch of its own.
        let graph = reference::graph(n, 1, &path(n), true, reverse);
        let precision = if f64s { Precision::F64 } else { Precision::F32 };
        let options = EigenOptions { precision, tol: 1e-3, max_iter: Some(300), ..EigenOptions::new(k) };
        let bytes = path_working_set(n, operator, k, precision).bytes();
        let solved = solve_within(bytes, &graph, &GraphMatrix::new(operator), &options);
        prop_assert!(solved.is_ok(), "{solved:?}");
    }
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

#[test]
fn restarts_keep_converged_pairs_and_converge_in_few_steps() {
    // Keeping the k wanted Ritz vectors plus one more per converged pair
    // (ARPACK's adjustment, clamped to a quarter to half of the free space)
    // takes 292 steps here; keeping k + (m − k)/2 regardless took 327.
    let edges = random_graph(2000, 8000, 21);
    let graph = reference::graph(2000, 2, &edges, true, true);
    let options = EigenOptions { which: Which::Smallest, tol: 1e-6, ..EigenOptions::new(10) };
    let eigen = solve(&graph, Operator::NormalizedLaplacian, &options);
    assert!(eigen.converged);
    assert!(eigen.iterations <= 300, "{} steps", eigen.iterations);
}

/// `count` disjoint undirected cycles of `length`.
fn cycles(count: u64, length: u64) -> Edges {
    (0..count * length)
        .flat_map(|i| {
            let next = i - i % length + (i + 1) % length;
            [(0, i, next, 1.0), (0, next, i, 1.0)]
        })
        .collect()
}

#[test]
fn of_two_eigenvalues_of_equal_magnitude_the_positive_comes_first() {
    // An even cycle's adjacency has eigenvalues 2 and −2 (30 times each
    // here); which Ritz value is larger in magnitude is rounding.
    let mut edges = crate::graph::EdgeList::for_id_space(1200, false);
    let relation = reference::relations(1)[0];
    for (_, source, target, _) in cycles(30, 40) {
        edges.push(relation, source, target, 1.0).unwrap();
    }
    let graph = GraphCsr::build(edges, 1, 1200, false).unwrap();
    for (dtype, symmetrize) in
        [(Precision::F64, false), (Precision::F64, true), (Precision::F32, false)]
    {
        let options = EigenOptions { precision: dtype, ..EigenOptions::new(3) };
        let matrix = GraphMatrix { symmetrize, ..GraphMatrix::new(Operator::Adjacency) };
        let solved = solve_on(2, &graph, &matrix, &options).unwrap();
        let values: Vec<f64> = solved.pairs.iter().map(|p| p.value).collect();
        assert!((values[0] - 2.0).abs() < 1e-5, "{values:?}");
        for pair in values.windows(2) {
            let tie = (pair[0].abs() - pair[1].abs()).abs() < 1e-5;
            assert!(!tie || pair[0] >= pair[1], "{dtype:?}: {values:?}");
        }
    }
}

#[test]
fn the_connected_components_of_the_active_subgraph_are_counted() {
    let cycles = reference::graph(1201, 1, &cycles(30, 40), false, true);
    let eigen = solve(&cycles, Operator::Laplacian, &EigenOptions::new(1));
    assert_eq!(eigen.components, 30, "node 1200 is isolated, so inactive");
    let one = reference::graph(10, 1, &path(10), false, true);
    assert_eq!(solve(&one, Operator::Laplacian, &EigenOptions::new(1)).components, 1);
}

/// The unit vector over `n` rows that is `weight(node)` on `nodes`, 0 elsewhere.
fn component_vector(n: u64, nodes: std::ops::Range<u64>, weight: impl Fn(u64) -> f64) -> Vec<f64> {
    let norm = nodes.clone().map(|i| weight(i).powi(2)).sum::<f64>().sqrt();
    (0..n).map(|i| if nodes.contains(&i) { weight(i) / norm } else { 0.0 }).collect()
}

#[test]
fn every_copy_of_a_component_eigenvalue_is_found_exactly() {
    // 30 disjoint 40-cycles: both Laplacians have eigenvalue 0 thirty times
    // and the random walk 1. Lanczos from one start vector found one copy
    // and returned [0, 0.025, 0.098] for the 3 smallest Laplacian
    // eigenvalues; on roadNet-TX's 424 components it ground on past the
    // statement timeout (DESIGN.md §12). Each copy is one component's
    // vector, lowest dense id first, found without a Lanczos step.
    let graph = reference::graph(1201, 1, &cycles(30, 40), false, true);
    for (operator, which, value) in [
        (Operator::Laplacian, Which::Smallest, 0.0),
        (Operator::NormalizedLaplacian, Which::Smallest, 0.0),
        (Operator::RandomWalk, Which::Largest, 1.0),
    ] {
        let eigen = solve(&graph, operator, &EigenOptions { which, ..EigenOptions::new(20) });
        assert!(eigen.converged, "{operator:?}");
        assert_eq!(eigen.iterations, 0, "{operator:?}");
        assert_eq!(eigen.components, 30);
        assert_eq!(eigen.pairs.len(), 20);
        for (c, pair) in (0..).zip(&eigen.pairs) {
            assert_eq!(pair.value, value, "{operator:?} pair {c}");
            assert!(pair.residual < 1e-6, "{operator:?} pair {c}: residual {}", pair.residual);
            let exact = component_vector(1201, 40 * c..40 * (c + 1), |_| 1.0);
            for (got, want) in as_f64(&pair.vector).iter().zip(&exact) {
                assert!((got - want).abs() < 1e-7, "{operator:?} pair {c}: {got} != {want}");
            }
        }
    }
}

#[test]
fn the_normalised_laplacian_s_zero_eigenvectors_weigh_each_node_by_its_root_degree() {
    // Two stars: each component's vector is D^1/2 times its indicator.
    let star = |centre: u64, leaves: u64| -> Edges {
        (1..=leaves)
            .flat_map(|l| [(0, centre, centre + l, 1.0), (0, centre + l, centre, 1.0)])
            .collect()
    };
    let edges: Edges = star(0, 3).into_iter().chain(star(4, 5)).collect();
    let graph = reference::graph(10, 1, &edges, false, true);
    let options =
        EigenOptions { which: Which::Smallest, precision: Precision::F64, ..EigenOptions::new(2) };
    let eigen = solve(&graph, Operator::NormalizedLaplacian, &options);
    let root_degree = |centre: u64, leaves: u64| {
        move |i: u64| match i == centre {
            true => (leaves as f64).sqrt(),
            false => 1.0,
        }
    };
    let expected = [
        component_vector(10, 0..4, root_degree(0, 3)),
        component_vector(10, 4..10, root_degree(4, 5)),
    ];
    for (pair, exact) in eigen.pairs.iter().zip(&expected) {
        assert_eq!(pair.value, 0.0);
        for (got, want) in as_f64(&pair.vector).iter().zip(exact) {
            assert!((got - want).abs() < 1e-15, "{got} != {want}");
        }
    }
}

/// Disjoint random graphs of the given sizes (each connected, so each a
/// component), numbered consecutively from 0; the id space and the edges.
fn components_of(sizes: &[u64], seed: u64) -> (u64, Edges) {
    let mut edges = Edges::new();
    let mut first = 0;
    for (c, &size) in (0..).zip(sizes) {
        let component = random_graph(size, 2 * size as usize, seed.wrapping_add(c));
        edges.extend(component.into_iter().map(|(r, s, t, w)| (r, first + s, first + t, w)));
        first += size;
    }
    (first, edges)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn with_fewer_components_than_k_the_rest_come_from_lanczos(
        sizes in prop::collection::vec(4u64..30, 2..5),
        seed in any::<u64>(),
        extra in 1usize..4,
        case in 0usize..3,
        f64_vectors in any::<bool>(),
    ) {
        let (operator, which) = [
            (Operator::Laplacian, Which::Smallest),
            (Operator::NormalizedLaplacian, Which::Smallest),
            (Operator::RandomWalk, Which::Largest),
        ][case];
        let (n, edges) = components_of(&sizes, seed);
        let graph = reference::graph(n, 2, &edges, true, seed % 2 == 0);
        let options = EigenOptions {
            which,
            precision: if f64_vectors { Precision::F64 } else { Precision::F32 },
            seed,
            tol: 1e-8,
            ..EigenOptions::new(sizes.len() + extra)
        };
        let eigen = solve(&graph, operator, &options);
        prop_assert_eq!(eigen.components, sizes.len() as u64);
        check(n, &edges, operator, &options, &eigen)?;
    }
}

#[test]
fn the_components_are_those_of_the_relations_selected() {
    // Relation 0 is a path through all ten nodes; relation 1 links 0 – 1,
    // 2 – 3 and 4 – 5 (and 0 – 1 again). Over relation 1 alone six nodes
    // are active, in three components; over both, one component.
    let mut edges = path(10);
    edges.extend(
        [(0, 1), (2, 3), (4, 5)].iter().flat_map(|&(s, t)| [(1, s, t, 1.0), (1, t, s, 1.0)]),
    );
    let graph = reference::graph(10, 2, &edges, false, true);
    let relations = reference::relations(2);
    let only = [relations[1]];
    for per_relation in [false, true] {
        let matrix = GraphMatrix {
            relations: RelationSelection::Only(&only),
            per_relation,
            ..GraphMatrix::new(Operator::NormalizedLaplacian)
        };
        let options = EigenOptions { which: Which::Smallest, ..EigenOptions::new(3) };
        let eigen = solve_on(2, &graph, &matrix, &options).unwrap();
        assert_eq!((eigen.components, eigen.iterations), (3, 0));
        for (c, pair) in (0..).zip(&eigen.pairs) {
            assert_eq!(pair.value, 0.0);
            let exact = component_vector(10, 2 * c..2 * c + 2, |_| 1.0);
            for (got, want) in as_f64(&pair.vector).iter().zip(&exact) {
                assert!((got - want).abs() < 1e-7, "pair {c}: {got} != {want}");
            }
        }
        let all = GraphMatrix { per_relation, ..GraphMatrix::new(Operator::NormalizedLaplacian) };
        let eigen = solve_on(2, &graph, &all, &options).unwrap();
        assert_eq!(eigen.components, 1);
        assert_eq!(eigen.pairs[0].value, 0.0);
        assert!(eigen.pairs[1].value > 0.01, "{}", eigen.pairs[1].value);
    }
}

#[test]
fn a_negative_weight_puts_an_eigenvalue_below_the_components_zero() {
    // Component 0 – 1 – 2, weights 1 and −3, has a negative Laplacian
    // eigenvalue; component 3 – 4 has 0 and 2. Both zeros are still exact,
    // after the negative eigenvalue.
    let edges: Edges = [(0, 1, 1.0), (1, 2, -3.0), (3, 4, 1.0)]
        .into_iter()
        .flat_map(|(s, t, w)| [(0, s, t, w), (0, t, s, w)])
        .collect();
    let graph = reference::graph(5, 1, &edges, true, true);
    let options = EigenOptions {
        which: Which::Smallest,
        precision: Precision::F64,
        tol: 1e-10,
        ..EigenOptions::new(3)
    };
    let eigen = solve(&graph, Operator::Laplacian, &options);
    check(5, &edges, Operator::Laplacian, &options, &eigen).unwrap();
    assert!(eigen.pairs[0].value < -1.0, "{}", eigen.pairs[0].value);
    assert_eq!([eigen.pairs[1].value, eigen.pairs[2].value], [0.0, 0.0]);
}

#[test]
fn results_with_components_are_identical_for_every_thread_count() {
    let (n, edges) = components_of(&[40, 25, 60, 9], 13);
    let graph = reference::graph(n, 2, &edges, true, false);
    for (operator, which) in
        [(Operator::NormalizedLaplacian, Which::Smallest), (Operator::RandomWalk, Which::Largest)]
    {
        let options = EigenOptions { which, seed: 5, ..EigenOptions::new(7) };
        let matrix = GraphMatrix::new(operator);
        let runs: Vec<Eigen> =
            (1..=4).map(|t| solve_on(t, &graph, &matrix, &options).unwrap()).collect();
        for run in &runs[1..] {
            assert_eq!(run, &runs[0]);
        }
    }
}

#[test]
fn a_loose_tolerance_does_not_reorder_distinct_magnitudes() {
    // S = [[−0.05, b], [b, −0.05]] with b = √(0.05² + 3.8): eigenvalues −2
    // and 1.9. With tol 0.1 the largest is still −2.
    let b = (0.05f64.powi(2) + 3.8).sqrt();
    let edges = vec![(0, 0, 0, -0.05), (0, 0, 1, b), (0, 1, 0, b), (0, 1, 1, -0.05)];
    let graph = reference::graph(2, 1, &edges, true, true);
    let options = EigenOptions { tol: 0.1, precision: Precision::F64, ..EigenOptions::new(1) };
    let value = solve(&graph, Operator::Adjacency, &options).pairs[0].value;
    assert!((value + 2.0).abs() < 1e-9, "{value}");
}

#[test]
fn the_default_step_limit_is_a_thousand_steps_per_basis_vector() {
    // 100 per basis vector stopped a 1M-node random graph's normalised
    // Laplacian short of the ~4,300 steps it needs (DESIGN.md §6.5).
    let edges = random_graph(60, 120, 4);
    let graph = reference::graph(60, 2, &edges, true, true);
    let options = EigenOptions { which: Which::Smallest, tol: 0.0, ..EigenOptions::new(2) };
    let eigen = solve(&graph, Operator::NormalizedLaplacian, &options);
    assert!(!eigen.converged);
    assert_eq!(eigen.iterations, 1000 * 20);
}

#[test]
fn the_default_tolerance_is_near_the_rounding_of_f32_vectors() {
    // Their explicit residual floors near 1e-7·‖M‖, so a tighter default
    // would buy steps, not accuracy (DESIGN.md §6.5).
    assert_eq!(EigenOptions::new(3).tol, 1e-6);
}

fn solve_reported(
    graph: &GraphCsr,
    operator: Operator,
    options: &EigenOptions,
) -> (Eigen, Recorded) {
    let memory = ComputeMemory::new(1 << 26);
    let mut recorded = Recorded::default();
    let mut resources =
        Resources { workers: workers(2), memory: &memory, interrupts: &mut recorded };
    let eigen = eigen(&mut resources, graph, &GraphMatrix::new(operator), options).unwrap();
    (eigen, recorded)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn lanczos_reports_its_steps_of_max_iter_and_reporting_changes_nothing(
        n in 8u64..120,
        density in 0usize..4,
        seed in any::<u64>(),
        k in 1usize..4,
        max_iter in prop::option::of(1usize..60),
        operator in operators(),
    ) {
        let edges = random_graph(n, density * n as usize, seed);
        let graph = reference::graph(n, 2, &edges, true, seed % 2 == 0);
        let options = EigenOptions { which: Which::Smallest, seed, max_iter, ..EigenOptions::new(k) };
        let (eigen, recorded) = solve_reported(&graph, operator, &options);
        prop_assert_eq!(&eigen, &solve_on(2, &graph, &GraphMatrix::new(operator), &options).unwrap());
        // The graph is connected, so the Laplacians' eigenvalue 0 is its one
        // component's: Lanczos computes the other k − 1 pairs over n − 1
        // dimensions, and does not run for k = 1.
        let deflated = matches!(operator, Operator::Laplacian | Operator::NormalizedLaplacian);
        let (wanted, dimension) = match deflated {
            true => (k - 1, n as usize - 1),
            false => (k, n as usize),
        };
        if wanted == 0 {
            prop_assert!(recorded.phases().is_empty());
            prop_assert_eq!(eigen.iterations, 0);
            return Ok(());
        }
        let total = max_iter.unwrap_or(1000 * 20.max(2 * k + 1).min(dimension)).max(k) as u64;
        prop_assert_eq!(recorded.phases(), vec![("lanczos", Some(total))]);
        prop_assert_eq!(recorded.checked_done(), vec![eigen.iterations as u64]);
        let converged = recorded.last_detail("converged").unwrap();
        prop_assert!(converged <= k as f64);
        prop_assert!(!eigen.converged || converged == k as f64);
        prop_assert!(recorded.last_detail("restarts").unwrap() >= 0.0);
    }
}

#[test]
fn a_restarting_solve_reports_its_restarts_and_largest_residual() {
    let n = 200;
    let edges = random_graph(n, 600, 7);
    let graph = reference::graph(n, 2, &edges, true, true);
    let options = EigenOptions { which: Which::Smallest, ..EigenOptions::new(3) };
    let (eigen, recorded) = solve_reported(&graph, Operator::NormalizedLaplacian, &options);
    assert!(eigen.converged);
    assert!(recorded.last_detail("restarts").unwrap() >= 1.0);
    assert_eq!(recorded.last_detail("converged"), Some(3.0));
    let residual = recorded.last_detail("residual").unwrap();
    assert!(residual.is_finite() && residual >= 0.0);
}

/// Disjoint undirected paths of the given lengths, numbered consecutively
/// from 0; the id space and the edges.
fn paths(lengths: &[u64]) -> (u64, Edges) {
    let mut edges = Edges::new();
    let mut first = 0;
    for &length in lengths {
        edges.extend(path(length).into_iter().map(|(r, s, t, w)| (r, first + s, first + t, w)));
        first += length;
    }
    (first, edges)
}

/// The unit vector over `n` rows alternating `+1, −1, …` on `nodes` from
/// its first, times `weight(node)`, 0 elsewhere: a bipartite component's
/// two colours.
fn alternating_vector(
    n: u64,
    nodes: std::ops::Range<u64>,
    weight: impl Fn(u64) -> f64,
) -> Vec<f64> {
    let first = nodes.start;
    let unit = component_vector(n, nodes, weight);
    (0..n).zip(unit).map(|(i, u)| if (i.wrapping_sub(first)) % 2 == 0 { u } else { -u }).collect()
}

#[test]
fn every_bipartite_component_s_minus_one_is_found_exactly() {
    // Paths and even cycles are bipartite: each gives the random walk the
    // eigenvalue −1 once, as large in magnitude as its 1. Lanczos found one
    // copy and returned [1, 1, 1, 1, −1, 0.9709, −0.9709] for four paths,
    // without a warning. Each copy is one component's colouring, the
    // positive first among equal magnitudes.
    let (n, edges) = paths(&[5, 8, 11, 14]);
    let graph = reference::graph(n, 1, &edges, false, true);
    let options =
        EigenOptions { which: Which::Largest, precision: Precision::F64, ..EigenOptions::new(7) };
    let eigen = solve(&graph, Operator::RandomWalk, &options);
    let values: Vec<f64> = eigen.pairs.iter().map(|p| p.value).collect();
    assert_eq!(values, [1.0, 1.0, 1.0, 1.0, -1.0, -1.0, -1.0]);
    assert_eq!((eigen.iterations, eigen.exact, eigen.components), (0, 7, 4));
    for (pair, nodes) in eigen.pairs[4..].iter().zip([0..5, 5..13, 13..24]) {
        let exact = alternating_vector(n, nodes, |_| 1.0);
        for (got, want) in as_f64(&pair.vector).iter().zip(&exact) {
            assert!((got - want).abs() < 1e-12, "{got} != {want}");
        }
        assert!(pair.residual < 1e-12, "{}", pair.residual);
    }
    let cycles = reference::graph(120, 1, &cycles(3, 40), false, true);
    let six = solve(&cycles, Operator::RandomWalk, &EigenOptions { k: 6, ..options });
    let values: Vec<f64> = six.pairs.iter().map(|p| p.value).collect();
    assert_eq!(values, [1.0, 1.0, 1.0, -1.0, -1.0, -1.0]);
    // Past them, Lanczos runs on the rest.
    let eight = EigenOptions { k: 8, tol: 1e-10, ..options };
    let rest = solve(&cycles, Operator::RandomWalk, &eight);
    check(120, &self::cycles(3, 40), Operator::RandomWalk, &eight, &rest).unwrap();
    assert_eq!(rest.exact, 6);
}

#[test]
fn the_normalised_laplacian_s_two_is_found_once_per_bipartite_component() {
    // Its largest eigenvalue is 2 once per bipartite component, with
    // eigenvector D^1/2 times the colouring; a triangle has none.
    let (n, mut edges) = paths(&[5, 8, 11]);
    edges.extend(
        [(24, 25), (25, 26), (26, 24)].iter().flat_map(|&(s, t)| [(0, s, t, 1.0), (0, t, s, 1.0)]),
    );
    let n = n + 3;
    let graph = reference::graph(n, 1, &edges, false, true);
    let options =
        EigenOptions { which: Which::Largest, precision: Precision::F64, ..EigenOptions::new(3) };
    let eigen = solve(&graph, Operator::NormalizedLaplacian, &options);
    let values: Vec<f64> = eigen.pairs.iter().map(|p| p.value).collect();
    assert_eq!(values, [2.0, 2.0, 2.0]);
    assert_eq!((eigen.iterations, eigen.exact), (0, 3));
    for (pair, (first, length)) in eigen.pairs.iter().zip([(0, 5), (5, 8), (13, 11)]) {
        let root_degree =
            |i: u64| if i == first || i == first + length - 1 { 1.0 } else { 2f64.sqrt() };
        let exact = alternating_vector(n, first..first + length, root_degree);
        let dot: f64 = as_f64(&pair.vector).iter().zip(&exact).map(|(a, b)| a * b).sum();
        assert!((dot.abs() - 1.0).abs() < 1e-12, "{dot}");
    }
    let four = EigenOptions { k: 4, tol: 1e-10, ..options };
    let rest = solve(&graph, Operator::NormalizedLaplacian, &four);
    check(n, &edges, Operator::NormalizedLaplacian, &four, &rest).unwrap();
}

#[test]
fn a_component_whose_lowest_node_is_inactive_is_still_counted() {
    // Unsymmetrised, a node is active by its forward row: 1 → 0 of weight
    // 0 leaves 0 inactive yet linked to 1, the lowest node of {0, 1, 2}.
    // Its component was numbered by 0 and went uncounted, and the
    // deflation indexed past its coefficients.
    let laplacian = GraphMatrix { symmetrize: false, ..GraphMatrix::new(Operator::Laplacian) };
    let options =
        EigenOptions { which: Which::Smallest, precision: Precision::F64, ..EigenOptions::new(1) };
    for weight in [0.0, 1e-12] {
        let edges: Edges = vec![(0, 1, 2, 1.0), (0, 2, 1, 1.0), (0, 1, 0, weight)];
        let graph = reference::graph(3, 1, &edges, true, true);
        let eigen = solve_on(1, &graph, &laplacian, &options).unwrap();
        assert_eq!((eigen.components, eigen.pairs[0].value), (1, 0.0), "weight {weight}");
        let exact = component_vector(3, 1..3, |_| 1.0);
        for (got, want) in as_f64(&eigen.pairs[0].vector).iter().zip(&exact) {
            assert!((got - want).abs() < 1e-15, "{got} != {want}");
        }
    }
    // Nor are two components joined through an inactive node: {0, 3} and
    // {2, 4}, with 2 → 1 of weight 0.
    let edges: Edges =
        vec![(0, 0, 3, 1.0), (0, 3, 0, 1.0), (0, 2, 4, 1.0), (0, 4, 2, 1.0), (0, 2, 1, 0.0)];
    let graph = reference::graph(5, 1, &edges, true, true);
    let eigen = solve_on(1, &graph, &laplacian, &EigenOptions { k: 2, ..options }).unwrap();
    assert_eq!(eigen.components, 2);
    for (pair, nodes) in eigen.pairs.iter().zip([[0, 3], [2, 4]]) {
        assert_eq!(pair.value, 0.0);
        let exact: Vec<f64> =
            (0..5).map(|i| if nodes.contains(&i) { 0.5f64.sqrt() } else { 0.0 }).collect();
        for (got, want) in as_f64(&pair.vector).iter().zip(&exact) {
            assert!((got - want).abs() < 1e-15, "{got} != {want}");
        }
    }
}

#[test]
fn the_exact_pairs_come_first_among_values_equal_to_rounding() {
    // Lanczos's zeros may round below 0; the components' exact zeros still
    // come first, and a value distinct beyond rounding does not move.
    let memory = ComputeMemory::new(1 << 20);
    let pair = |value: f64| Eigenpair {
        value,
        residual: 0.0,
        vector: Vector::<f64>::filled(&memory, 1, 1, value).unwrap().into(),
    };
    let known = vec![pair(0.0), pair(0.0)];
    let found = vec![pair(-2.9e-17), pair(-2.0e-17), pair(-1e-3)];
    let (merged, exact) = merge(known, found, 4, Which::Smallest, F64_TIE);
    assert_eq!(exact, 2);
    let values: Vec<f64> = merged.iter().map(|p| p.value).collect();
    assert_eq!(values, [-1e-3, 0.0, 0.0, -2.9e-17]);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn bipartite_components_give_the_largest_end_and_lanczos_the_rest(
        // A path of n nodes has random-walk eigenvalues cos(πj/(n − 1)):
        // with n − 1 distinct primes, paths share only ±1, which Lanczos
        // would otherwise have to find more than once.
        lengths in prop::sample::subsequence(vec![3u64, 4, 6, 8, 12, 14], 1..5).prop_shuffle(),
        extra in 1usize..4,
        normalized in any::<bool>(),
        f64_vectors in any::<bool>(),
        seed in any::<u64>(),
    ) {
        let (n, edges) = paths(&lengths);
        let operator = match normalized {
            true => Operator::NormalizedLaplacian,
            false => Operator::RandomWalk,
        };
        let known = if normalized { lengths.len() } else { 2 * lengths.len() };
        let k = (known + extra).min(n as usize);
        let graph = reference::graph(n, 1, &edges, false, seed % 2 == 0);
        let options = EigenOptions {
            precision: if f64_vectors { Precision::F64 } else { Precision::F32 },
            seed,
            tol: 1e-8,
            ..EigenOptions::new(k)
        };
        let eigen = solve(&graph, operator, &options);
        prop_assert_eq!(eigen.exact, known.min(k));
        check(n, &edges, operator, &options, &eigen)?;
        let runs: Vec<Eigen> =
            (1..=4).map(|t| solve_on(t, &graph, &GraphMatrix::new(operator), &options).unwrap()).collect();
        for run in &runs[1..] {
            prop_assert_eq!(run, &runs[0]);
        }
    }
}

#[test]
fn an_f32_residual_at_rounding_level_is_a_breakdown_not_a_direction() {
    // Three paths' normalised Laplacian, its three 2s deflated: the
    // complement holds 0 three times, so the Krylov space of one start
    // vector is invariant after 13 of its 15 dimensions. In f32 the 13th
    // residual was rounding (1.8·10⁻⁶ of what it came from), above the f64
    // breakdown threshold: normalised, it took a basis vector, the basis
    // "filled" the complement, and the solve stopped as converged with a
    // Ritz value of 1.92 (residual 0.23) for 1.901.
    let (n, edges) = paths(&[4, 6, 8]);
    let graph = reference::graph(n, 1, &edges, false, true);
    for seed in [1, 2, 3] {
        let options = EigenOptions { precision: Precision::F32, seed, ..EigenOptions::new(4) };
        let eigen = solve(&graph, Operator::NormalizedLaplacian, &options);
        check(n, &edges, Operator::NormalizedLaplacian, &options, &eigen).unwrap();
    }
}
