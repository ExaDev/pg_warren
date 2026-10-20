//! The projected eigenproblem of a Lanczos iteration (Rayleigh–Ritz): the
//! eigenpairs of the small dense symmetric matrix `H = QᵀMQ`, solved by
//! nalgebra and ordered by the end of the spectrum wanted.

use std::cmp::Ordering;

use nalgebra::{DMatrix, DMatrixView, SymmetricEigen};

use super::working_set::RITZ_MATRICES;
use super::{SpectralError, Which};
use crate::compute::{ComputeMemory, MemoryCharge};

/// The sweeps nalgebra's symmetric QR iteration may take per dimension.
const SWEEPS_PER_DIMENSION: usize = 100;

/// Ritz values and the coordinates of their Ritz vectors in the basis.
#[derive(Debug)]
pub(super) struct Ritz {
    /// Ritz values in `which` order.
    pub(super) values: Vec<f64>,
    /// Column `l` holds the coordinates of the Ritz vector of `values[l]`.
    pub(super) vectors: DMatrix<f64>,
    /// The decomposition's matrices and values, charged for as long as
    /// the pairs are held.
    _charge: MemoryCharge,
}

impl Ritz {
    /// The eigenpairs of the symmetric `h`, in `which` order. For
    /// `Largest`, values whose magnitudes differ by at most `tie · max|θ|`
    /// count as equal, so that `λ` comes before `−λ` whichever rounding
    /// made larger. The copy of `h` it decomposes, the eigenvectors and
    /// them reordered are charged to `memory` before any is made.
    pub(super) fn of(
        memory: &ComputeMemory,
        h: &DMatrixView<'_, f64>,
        which: Which,
        tie: f64,
    ) -> Result<Self, SpectralError> {
        if h.iter().any(|value| !value.is_finite()) {
            return Err(SpectralError::NotFinite);
        }
        let n = h.nrows();
        let charge = memory.charge_elements::<f64>(RITZ_MATRICES as usize * n * n + n)?;
        let h = h.into_owned();
        let eigen = SymmetricEigen::try_new(h, f64::EPSILON, SWEEPS_PER_DIMENSION * n.max(1))
            .ok_or(SpectralError::NotFinite)?;
        let mut order: Vec<usize> = (0..n).collect();
        let values = eigen.eigenvalues.as_slice();
        order.sort_by(|&a, &b| compare(which, values[a], values[b]).then(a.cmp(&b)));
        if which == Which::Largest {
            positives_first(&mut order, values, tie);
        }
        let vectors = DMatrix::from_fn(n, n, |i, l| eigen.eigenvectors[(i, order[l])]);
        Ok(Self { values: order.iter().map(|&l| values[l]).collect(), vectors, _charge: charge })
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
    use crate::compute::ComputeError;

    fn ritz(h: DMatrix<f64>, which: Which, tie: f64) -> Result<Ritz, SpectralError> {
        Ritz::of(&ComputeMemory::new(1 << 20), &h.as_view(), which, tie)
    }

    #[test]
    fn the_decomposition_is_charged_while_its_pairs_are_held() {
        let h = DMatrix::from_diagonal(&nalgebra::DVector::from_vec(vec![1.0, 2.0, 3.0]));
        let bytes = (RITZ_MATRICES * 9 + 3) * 8;
        let memory = ComputeMemory::new(bytes);
        let pairs = Ritz::of(&memory, &h.as_view(), Which::Largest, 0.0).unwrap();
        assert_eq!(memory.in_use(), bytes);
        drop(pairs);
        let short = ComputeMemory::new(bytes - 1);
        let refused = Ritz::of(&short, &h.as_view(), Which::Largest, 0.0).unwrap_err();
        assert!(matches!(refused, SpectralError::Compute(ComputeError::MemoryLimit { .. })));
    }

    #[test]
    fn ritz_pairs_come_in_which_order() {
        let h = DMatrix::from_diagonal(&nalgebra::DVector::from_vec(vec![1.0, -3.0, 2.0, 3.0]));
        let largest = ritz(h.clone(), Which::Largest, 0.0).unwrap();
        assert_eq!(largest.values, [3.0, -3.0, 2.0, 1.0]);
        assert_eq!(
            largest.vectors.column(0).iter().copied().collect::<Vec<_>>(),
            [0.0, 0.0, 0.0, 1.0]
        );
        let smallest = ritz(h, Which::Smallest, 0.0).unwrap();
        assert_eq!(smallest.values, [-3.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn values_of_equal_magnitude_within_the_tie_tolerance_put_the_positive_first() {
        // −2 is larger in magnitude by rounding only.
        let h = DMatrix::from_diagonal(&nalgebra::DVector::from_vec(vec![1.0, -2.0 - 4e-15, 2.0]));
        assert_eq!(
            ritz(h.clone(), Which::Largest, 1e-12).unwrap().values,
            [2.0, -2.0 - 4e-15, 1.0]
        );
        let apart = DMatrix::from_diagonal(&nalgebra::DVector::from_vec(vec![-2.1, 2.0]));
        assert_eq!(ritz(apart, Which::Largest, 1e-12).unwrap().values, [-2.1, 2.0]);
    }

    fn largest(values: &[f64], tie: f64) -> Vec<f64> {
        let h = DMatrix::from_diagonal(&nalgebra::DVector::from_vec(values.to_vec()));
        ritz(h, Which::Largest, tie).unwrap().values
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
        assert_eq!(ritz(h, Which::Largest, 0.0).unwrap_err(), SpectralError::NotFinite);
    }
}
