//! The `spectral` distance: how far apart two eigenvalue sequences are.

/// The difference between two top-k eigenvalue sequences, compared in
/// order; a missing eigenvalue counts as 0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpectralDistance {
    /// `‖λ − μ‖₂`.
    pub l2: f64,
    /// `‖λ − μ‖₂ / max(‖λ‖₂, ‖μ‖₂)` (0 when both are zero).
    pub relative: f64,
    /// `max |λᵢ − μᵢ|`.
    pub max: f64,
}

/// The distance between eigenvalue sequences `before` and `after`, each in
/// the order the eigensolver returned them.
pub fn spectral_distance(before: &[f64], after: &[f64]) -> SpectralDistance {
    let len = before.len().max(after.len());
    let at = |values: &[f64], i: usize| values.get(i).copied().unwrap_or(0.0);
    let differences = (0..len).map(|i| (at(before, i) - at(after, i)).abs());
    let (squares, max) = differences.fold((0.0, 0.0f64), |(s, m), d| (s + d * d, m.max(d)));
    let norm = |values: &[f64]| values.iter().map(|v| v * v).sum::<f64>().sqrt();
    let l2 = f64::sqrt(squares);
    let scale = norm(before).max(norm(after));
    SpectralDistance { l2, relative: if scale == 0.0 { 0.0 } else { l2 / scale }, max }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_sequences_are_zero_apart() {
        let d = spectral_distance(&[3.0, -1.0], &[3.0, -1.0]);
        assert_eq!(d, SpectralDistance { l2: 0.0, relative: 0.0, max: 0.0 });
        assert_eq!(
            spectral_distance(&[], &[]),
            SpectralDistance { l2: 0.0, relative: 0.0, max: 0.0 }
        );
    }

    #[test]
    fn sequences_compare_in_order_with_missing_values_zero() {
        let d = spectral_distance(&[3.0, 4.0], &[3.0, 1.0, 4.0]);
        assert_eq!(d.max, 4.0);
        assert_eq!(d.l2, 5.0);
        assert_eq!(d.relative, 5.0 / 26f64.sqrt());
        assert_eq!(spectral_distance(&[3.0, 1.0, 4.0], &[3.0, 4.0]), d);
    }
}
