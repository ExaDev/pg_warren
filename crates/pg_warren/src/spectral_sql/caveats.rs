//! What an eigensolve's caller should be warned of (DESIGN.md §6.5): that
//! it stopped before converging, and that the matrix has several connected
//! components, whose repeated eigenvalues single-vector Lanczos may not all
//! find, unless the eigenvalue each component gives was taken exactly from
//! the components.

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
    if solved.components > 1 && !solved.deflated {
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
    use super::*;

    fn solved(converged: bool, components: u64) -> Eigen {
        Eigen { pairs: Vec::new(), iterations: 25, converged, components, deflated: false }
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
        let deflated = Eigen { deflated: true, ..solved(true, 424) };
        assert!(caveats(&deflated).is_empty());
    }
}
