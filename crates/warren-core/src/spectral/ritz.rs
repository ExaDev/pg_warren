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
    /// The eigenpairs of the symmetric `h`, in `which` order. For
    /// `Largest`, values whose magnitudes differ by at most `tie · max|θ|`
    /// count as equal, so that `λ` comes before `−λ` whichever rounding
    /// made larger.
    pub(super) fn of(h: DMatrix<f64>, which: Which, tie: f64) -> Result<Self, SpectralError> {
        if h.iter().any(|value| !value.is_finite()) {
            return Err(SpectralError::NotFinite);
        }
        let n = h.nrows();
        let eigen = SymmetricEigen::try_new(h, f64::EPSILON, SWEEPS_PER_DIMENSION * n.max(1))
            .ok_or(SpectralError::NotFinite)?;
        let mut order: Vec<usize> = (0..n).collect();
        let values = eigen.eigenvalues.as_slice();
        order.sort_by(|&a, &b| compare(which, values[a], values[b]).then(a.cmp(&b)));
        if which == Which::Largest {
            positives_first(&mut order, values, tie);
        }
        let vectors = DMatrix::from_fn(n, n, |i, l| eigen.eigenvectors[(i, order[l])]);
        Ok(Self { values: order.iter().map(|&l| values[l]).collect(), vectors })
    }
}

/// Within each run of tied magnitudes in `order` (sorted by descending
/// magnitude), puts the positive values first, keeping each sign's order.
/// A run holds the values within `tie · max|θ|` of its first value's
/// magnitude, measured from that first value so that a run cannot drift.
fn positives_first(order: &mut [usize], values: &[f64], tie: f64) {
    let width = tie * values.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let mut start = 0;
    while start < order.len() {
        let first = values[order[start]].abs();
        let end = (start..order.len())
            .find(|&i| first - values[order[i]].abs() > width)
            .unwrap_or(order.len());
        order[start..end].sort_by_key(|&i| values[i] < 0.0);
        start = end;
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
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn ritz_pairs_come_in_which_order() {
        let h = DMatrix::from_diagonal(&nalgebra::DVector::from_vec(vec![1.0, -3.0, 2.0, 3.0]));
        let largest = Ritz::of(h.clone(), Which::Largest, 0.0).unwrap();
        assert_eq!(largest.values, [3.0, -3.0, 2.0, 1.0]);
        assert_eq!(
            largest.vectors.column(0).iter().copied().collect::<Vec<_>>(),
            [0.0, 0.0, 0.0, 1.0]
        );
        let smallest = Ritz::of(h, Which::Smallest, 0.0).unwrap();
        assert_eq!(smallest.values, [-3.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn values_of_equal_magnitude_within_the_tie_tolerance_put_the_positive_first() {
        // −2 is larger in magnitude by rounding only.
        let h = DMatrix::from_diagonal(&nalgebra::DVector::from_vec(vec![1.0, -2.0 - 4e-15, 2.0]));
        assert_eq!(
            Ritz::of(h.clone(), Which::Largest, 1e-12).unwrap().values,
            [2.0, -2.0 - 4e-15, 1.0]
        );
        let apart = DMatrix::from_diagonal(&nalgebra::DVector::from_vec(vec![-2.1, 2.0]));
        assert_eq!(Ritz::of(apart, Which::Largest, 1e-12).unwrap().values, [-2.1, 2.0]);
    }

    fn largest(values: &[f64], tie: f64) -> Vec<f64> {
        let h = DMatrix::from_diagonal(&nalgebra::DVector::from_vec(values.to_vec()));
        Ritz::of(h, Which::Largest, tie).unwrap().values
    }

    #[test]
    fn a_run_of_ties_puts_every_positive_first() {
        let run = [2.0 + 4e-10, -(2.0 + 3e-10), 2.0 + 2e-10, -(2.0 + 1e-10), 2.0];
        let ordered = largest(&run, 1e-6);
        assert_eq!(ordered, [2.0 + 4e-10, 2.0 + 2e-10, 2.0, -(2.0 + 3e-10), -(2.0 + 1e-10)]);
    }

    #[test]
    fn a_run_is_measured_from_its_first_value_and_cannot_drift() {
        // The tie is 0.04 · 3.3 = 0.132: each step of 0.1 is within it, but
        // 3.1 is not within it of 3.3, so it starts a run of its own rather
        // than being pulled ahead of −3.2.
        let ordered = largest(&[3.3, -3.2, 3.1, -3.0], 0.04);
        assert_eq!(ordered, [3.3, -3.2, 3.1, -3.0]);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn ties_order_positives_first_and_magnitudes_never_fall_out_of_order(
            values in prop::collection::vec((1u8..6, any::<bool>(), 0u8..4), 1..24),
        ) {
            // Magnitudes 1..5 apart by 1, signs random, each nudged below
            // the tie of 1e-9 (relative to the largest, 5).
            let values: Vec<f64> = values
                .iter()
                .map(|&(m, positive, nudge)| {
                    let magnitude = f64::from(m) + f64::from(nudge) * 1e-10;
                    if positive { magnitude } else { -magnitude }
                })
                .collect();
            let ordered = largest(&values, 1e-9);
            for pair in ordered.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                prop_assert!(a.abs() + 5e-9 >= b.abs(), "{:?}", ordered);
                let tied = (a.abs() - b.abs()).abs() < 0.5;
                prop_assert!(!tied || !(a < 0.0 && b > 0.0), "{:?}", ordered);
            }
        }
    }

    #[test]
    fn a_non_finite_matrix_is_refused() {
        let h = DMatrix::from_element(2, 2, f64::NAN);
        assert_eq!(Ritz::of(h, Which::Largest, 0.0).unwrap_err(), SpectralError::NotFinite);
    }
}
