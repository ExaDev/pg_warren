//! What an eigensolve's caller should be warned of (DESIGN.md §6.5): that
//! it stopped before converging, and that the matrix has several connected
//! components, whose repeated eigenvalues single-vector Lanczos may not all
//! find, unless every pair returned was taken exactly from the components.

use warren_core::spectral::Eigen;

/// The warnings `eigen` gives about `solved`.
pub(super) fn caveats(solved: &Eigen) -> Vec<String> {
    let mut warnings = Vec::new();
    if !solved.converged {
        let worst = solved.pairs.iter().map(|pair| pair.residual).fold(0.0f64, f64::max);
        warnings.push(format!(
            "eigen did not converge in {} steps; the largest residual is {worst:e} \
             (raise max_iter or tol)",
            solved.iterations
        ));
    }
    if solved.components > 1 && solved.exact < solved.pairs.len() {
        warnings.push(format!(
            "the matrix has {} connected components: eigenvalues repeat (each Laplacian has 0 \
             once per component), and single-vector Lanczos may return fewer copies of a \
             repeated eigenvalue than there are",
            solved.components
        ));
    }
    warnings
}

#[cfg(test)]
mod unit {
    use warren_core::compute::{ComputeMemory, Vector};
    use warren_core::spectral::Eigenpair;

    use super::*;

    /// A solve of `pairs` eigenpairs, the first `exact` of them taken
    /// from the components.
    fn solved_with(converged: bool, components: u64, pairs: usize, exact: usize) -> Eigen {
        let memory = ComputeMemory::new(1 << 10);
        let pair = || Eigenpair {
            value: 0.0,
            residual: 0.0,
            vector: Vector::<f64>::filled(&memory, 1, 1, 0.0).unwrap().into(),
        };
        let pairs = (0..pairs).map(|_| pair()).collect();
        Eigen { pairs, iterations: 25, converged, components, exact }
    }

    fn solved(converged: bool, components: u64) -> Eigen {
        solved_with(converged, components, 3, 0)
    }

    #[test]
    fn a_solve_that_did_not_converge_is_named() {
        let warnings = caveats(&solved(false, 1));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("eigen did not converge in 25 steps"), "{warnings:?}");
    }

    #[test]
    fn several_components_warn_of_missed_repeated_eigenvalues() {
        let warnings = caveats(&solved(true, 30));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("the matrix has 30 connected components"), "{warnings:?}");
        assert!(caveats(&solved(true, 1)).is_empty());
        assert_eq!(caveats(&solved(false, 2)).len(), 2);
    }

    #[test]
    fn components_whose_eigenvalue_was_taken_exactly_are_no_caveat() {
        // Every copy of the Laplacians' 0 (the random walk's 1) is one
        // component's eigenvector, found without Lanczos.
        assert!(caveats(&solved_with(true, 424, 20, 20)).is_empty());
    }

    #[test]
    fn pairs_from_lanczos_beside_the_components_still_warn() {
        // Other eigenvalues may repeat too (identical components, or the
        // adjacency's), and Lanczos may miss their copies.
        let warnings = caveats(&solved_with(true, 30, 31, 30));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("the matrix has 30 connected components"));
    }
}
