//! The `embedding` distance: how far the rows of two N×d matrices moved,
//! for example neighbour-aggregated embeddings before and after a change.
//!
//! Rows are aligned by dense id, a row past a matrix's id space is zero,
//! and rows that are zero in both matrices (nodes in neither version) are
//! left out. The cosine distance of a row is `1 − cos`, in `[0, 2]`, and 1
//! when exactly one of the two rows is zero.

use std::ops::Range;

use super::super::dense::{self, BLOCK};
use super::super::{Resources, SpectralError};
use super::rows::Rows;
use crate::compute::{AnyVector, ComputeError};
use crate::interrupts::Interrupts;

/// How far the rows of two matrices are apart.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmbeddingDistance {
    /// The mean Euclidean distance between corresponding rows.
    pub mean_l2: f64,
    /// The largest Euclidean distance between corresponding rows.
    pub max_l2: f64,
    /// The mean cosine distance between corresponding rows.
    pub mean_cosine: f64,
    /// The largest cosine distance between corresponding rows.
    pub max_cosine: f64,
    /// The rows compared: those non-zero in either matrix.
    pub rows: u64,
}

/// The `embedding` distance between `before` and `after`, `f32` or `f64`
/// matrices of the same width.
pub fn embedding_distance<I: Interrupts>(
    resources: &mut Resources<'_, I>,
    before: &AnyVector,
    after: &AnyVector,
) -> Result<EmbeddingDistance, SpectralError> {
    let (before, after) = (Rows::new(before)?, Rows::new(after)?);
    if before.width() != after.width() {
        let (expected, found) = (before.width(), after.width());
        return Err(ComputeError::WidthMismatch { expected, found }.into());
    }
    let rows = before.id_space().max(after.id_space()) as usize;
    let block = |range: Range<usize>| {
        range.fold(Totals::default(), |totals, row| totals.add_row(before, after, row as u64))
    };
    let totals = dense::fold(resources, rows, BLOCK, Totals::default(), block, Totals::combine)?;
    let mean = |sum: f64| if totals.rows == 0 { 0.0 } else { sum / totals.rows as f64 };
    Ok(EmbeddingDistance {
        mean_l2: mean(totals.l2),
        max_l2: totals.max_l2,
        mean_cosine: mean(totals.cosine),
        max_cosine: totals.max_cosine,
        rows: totals.rows,
    })
}

/// Sums and maxima over a run of rows.
#[derive(Debug, Clone, Copy, Default)]
struct Totals {
    l2: f64,
    max_l2: f64,
    cosine: f64,
    max_cosine: f64,
    rows: u64,
}

impl Totals {
    fn add_row(self, before: Rows<'_>, after: Rows<'_>, row: u64) -> Self {
        let columns = 0..before.width();
        let pairs = || columns.clone().map(|c| (before.get(row, c), after.get(row, c)));
        let (aa, bb, squares) = pairs().fold((0.0, 0.0, 0.0), |(aa, bb, s), (a, b)| {
            (aa + a * a, bb + b * b, s + (a - b) * (a - b))
        });
        if aa == 0.0 && bb == 0.0 {
            return self;
        }
        let l2 = f64::sqrt(squares);
        // `1 − cos = ‖a/‖a‖ − b/‖b‖‖² / 2`, which is exactly 0 for equal
        // rows and accurate for small angles.
        let (na, nb) = (f64::sqrt(aa), f64::sqrt(bb));
        let cosine = match aa == 0.0 || bb == 0.0 {
            true => 1.0,
            false => pairs().map(|(a, b)| (a / na - b / nb).powi(2)).sum::<f64>() / 2.0,
        };
        self.combine(Self { l2, max_l2: l2, cosine, max_cosine: cosine, rows: 1 })
    }

    fn combine(self, other: Self) -> Self {
        Self {
            l2: self.l2 + other.l2,
            max_l2: self.max_l2.max(other.max_l2),
            cosine: self.cosine + other.cosine,
            max_cosine: self.max_cosine.max(other.max_cosine),
            rows: self.rows + other.rows,
        }
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;
    use crate::compute::{ComputeError, ComputeMemory, Vector, Workers};
    use crate::interrupts::Uninterrupted;

    fn matrix<E: crate::compute::Element>(width: usize, values: Vec<E>) -> AnyVector
    where
        AnyVector: From<Vector<E>>,
    {
        let memory = ComputeMemory::new(1 << 20);
        Vector::from_values(&memory, (values.len() / width) as u64, width, values).unwrap().into()
    }

    fn distance(before: &AnyVector, after: &AnyVector) -> Result<EmbeddingDistance, SpectralError> {
        let workers = Workers::new(2).unwrap();
        let memory = ComputeMemory::new(1 << 20);
        let mut resources =
            Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted };
        embedding_distance(&mut resources, before, after)
    }

    #[test]
    fn identical_matrices_are_zero_apart() {
        let x = matrix(2, vec![1.0f64, 2.0, 0.0, 0.0, -3.0, 0.5]);
        let d = distance(&x, &x).unwrap();
        assert_eq!(
            d,
            EmbeddingDistance {
                mean_l2: 0.0,
                max_l2: 0.0,
                mean_cosine: 0.0,
                max_cosine: 0.0,
                rows: 2
            }
        );
    }

    #[test]
    fn rows_are_compared_by_dense_id() {
        // Row 0 turns by 90°, row 1 is zero in both, row 2 doubles, row 3
        // exists only after.
        let before = matrix(2, vec![1.0f32, 0.0, 0.0, 0.0, 3.0, 4.0]);
        let after = matrix(2, vec![0.0f64, 1.0, 0.0, 0.0, 6.0, 8.0, 0.0, 2.0]);
        let d = distance(&before, &after).unwrap();
        assert_eq!(d.rows, 3);
        assert!((d.mean_l2 - (2f64.sqrt() + 5.0 + 2.0) / 3.0).abs() < 1e-12);
        assert_eq!(d.max_l2, 5.0);
        assert!((d.mean_cosine - 2.0 / 3.0).abs() < 1e-12);
        assert_eq!(d.max_cosine, 1.0);
        assert_eq!(distance(&after, &before).unwrap(), d);
    }

    #[test]
    fn opposite_rows_are_two_apart_in_cosine() {
        let d = distance(&matrix(1, vec![2.0f64]), &matrix(1, vec![-1.0f64])).unwrap();
        assert_eq!((d.max_cosine, d.max_l2), (2.0, 3.0));
    }

    #[test]
    fn matrices_must_have_one_width() {
        let narrow = matrix(1, vec![1.0f64, 2.0]);
        let wide = matrix(2, vec![1.0f64, 2.0]);
        assert_eq!(
            distance(&narrow, &wide),
            Err(ComputeError::WidthMismatch { expected: 1, found: 2 }.into())
        );
        let empty = distance(&matrix(1, Vec::<f64>::new()), &matrix(1, Vec::<f64>::new())).unwrap();
        assert_eq!(empty.rows, 0);
        assert_eq!(empty.mean_l2, 0.0);
    }
}
