//! The projected eigenproblem of a Lanczos iteration (Rayleigh–Ritz): the
//! eigenpairs of the small dense symmetric matrix `H = QᵀMQ`, solved by
//! nalgebra and ordered by the end of the spectrum wanted.

use std::cmp::Ordering;

use nalgebra::{DMatrix, SymmetricEigen};

use super::{SpectralError, Which};

/// The sweeps nalgebra's symmetric QR iteration may take per dimension.
const SWEEPS_PER_DIMENSION: usize = 100;

/// Ritz values and the coordinates of their Ritz vectors in the basis.
#[derive(Debug, Clone)]
pub(super) struct Ritz {
    /// Ritz values in `which` order.
    pub(super) values: Vec<f64>,
    /// Column `l` holds the coordinates of the Ritz vector of `values[l]`.
    pub(super) vectors: DMatrix<f64>,
}

impl Ritz {
    /// The eigenpairs of the symmetric `h`, in `which` order.
    pub(super) fn of(h: DMatrix<f64>, which: Which) -> Result<Self, SpectralError> {
        if h.iter().any(|value| !value.is_finite()) {
            return Err(SpectralError::NotFinite);
        }
        let n = h.nrows();
        let eigen = SymmetricEigen::try_new(h, f64::EPSILON, SWEEPS_PER_DIMENSION * n.max(1))
            .ok_or(SpectralError::NotFinite)?;
        let mut order: Vec<usize> = (0..n).collect();
        let values = eigen.eigenvalues.as_slice();
        order.sort_by(|&a, &b| compare(which, values[a], values[b]).then(a.cmp(&b)));
        let vectors = DMatrix::from_fn(n, n, |i, l| eigen.eigenvectors[(i, order[l])]);
        Ok(Self { values: order.iter().map(|&l| values[l]).collect(), vectors })
    }
}

/// Whether `a` comes before `b` at the `which` end: largest magnitude
/// first (positive first on ties), or smallest first.
fn compare(which: Which, a: f64, b: f64) -> Ordering {
    match which {
        Which::Largest => b.abs().total_cmp(&a.abs()).then(b.total_cmp(&a)),
        Which::Smallest => a.total_cmp(&b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ritz_pairs_come_in_which_order() {
        let h = DMatrix::from_diagonal(&nalgebra::DVector::from_vec(vec![1.0, -3.0, 2.0, 3.0]));
        let largest = Ritz::of(h.clone(), Which::Largest).unwrap();
        assert_eq!(largest.values, [3.0, -3.0, 2.0, 1.0]);
        assert_eq!(
            largest.vectors.column(0).iter().copied().collect::<Vec<_>>(),
            [0.0, 0.0, 0.0, 1.0]
        );
        let smallest = Ritz::of(h, Which::Smallest).unwrap();
        assert_eq!(smallest.values, [-3.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn a_non_finite_matrix_is_refused() {
        let h = DMatrix::from_element(2, 2, f64::NAN);
        assert_eq!(Ritz::of(h, Which::Largest).unwrap_err(), SpectralError::NotFinite);
    }
}
