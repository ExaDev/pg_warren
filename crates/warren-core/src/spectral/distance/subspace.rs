//! The `subspace` distance: principal angles between the spans of two sets
//! of vectors, typically the top-k eigenvectors of two versions.
//!
//! The vectors need not be orthonormal, only linearly independent. One
//! pass over the rows computes the Gram matrix of both sets; with
//! `G_X = L_X L_Xᵀ` and `G_Y = L_Y L_Yᵀ` (Cholesky), the cosines of the
//! principal angles are the singular values of `L_X⁻¹ XᵀY L_Y⁻ᵀ`. Going
//! through the Gram matrix squares the condition number, which the
//! near-orthonormal eigenvectors this is for can afford; it limits the
//! resolution of small angles to about `√(Kε)` for `K` vectors, so cosines
//! within `8Kε` of 1 are taken as exactly 1 (angle 0). The pass uses a
//! fixed number of row blocks, so the result is the same for any thread
//! count.

use nalgebra::{DMatrix, SVD};

use super::super::{dense, Resources, SpectralError};
use super::rows::Rows;
use super::ComparedRows;
use crate::compute::{AnyVector, ComputeError};
use crate::interrupts::Interrupts;

/// The number of row blocks the Gram pass splits the rows into.
const GRAM_BLOCKS: usize = 64;

/// The sweeps nalgebra's SVD may take per dimension.
const SWEEPS_PER_DIMENSION: usize = 100;

/// How far apart two subspaces are.
#[derive(Debug, Clone, PartialEq)]
pub struct SubspaceDistance {
    /// `‖P_X − P_Y‖_F` for the orthogonal projections onto the two spans:
    /// `√(k₁ + k₂ − 2Σcos²θᵢ)`, between 0 and `√(k₁ + k₂)`.
    pub projection: f64,
    /// The largest principal angle, in radians (`π/2` at most).
    pub max_angle: f64,
    /// The `min(k₁, k₂)` principal angles, ascending, in radians.
    pub angles: Vec<f64>,
}

/// The `subspace` distance between the spans of `before` and `after`,
/// width-1 `f32` or `f64` vectors aligned by dense id, over the rows of
/// `compared`.
pub fn subspace_distance<I: Interrupts>(
    resources: &mut Resources<'_, I>,
    before: &[&AnyVector],
    after: &[&AnyVector],
    compared: ComparedRows,
) -> Result<SubspaceDistance, SpectralError> {
    if before.is_empty() || after.is_empty() {
        return Err(SpectralError::EmptySubspace);
    }
    let columns =
        before.iter().chain(after).map(|&v| Rows::new(v)).collect::<Result<Vec<_>, _>>()?;
    if let Some(wide) = columns.iter().find(|column| column.width() != 1) {
        return Err(ComputeError::WidthMismatch { expected: 1, found: wide.width() }.into());
    }
    let (k1, k2) = (before.len(), after.len());
    let gram = gram(resources, &columns, k1, compared)?;
    let cross = gram.view((0, k1), (k1, k2)).into_owned();
    let lx = factor(gram.view((0, 0), (k1, k1)).into_owned())?;
    let ly = factor(gram.view((k1, k1), (k2, k2)).into_owned())?;
    // M = L_X⁻¹ C L_Y⁻ᵀ = (L_Y⁻¹ (L_X⁻¹ C)ᵀ)ᵀ.
    let left = lx.solve_lower_triangular(&cross).ok_or(SpectralError::RankDeficient)?;
    let m = ly.solve_lower_triangular(&left.transpose()).ok_or(SpectralError::RankDeficient)?;
    let iterations = SWEEPS_PER_DIMENSION * k1.max(k2);
    let svd =
        SVD::try_new(m, false, false, f64::EPSILON, iterations).ok_or(SpectralError::NotFinite)?;
    let snap = 1.0 - 8.0 * (k1 + k2) as f64 * f64::EPSILON;
    let mut cosines: Vec<f64> = svd
        .singular_values
        .iter()
        .map(|&c| if c >= snap { 1.0 } else { c.clamp(0.0, 1.0) })
        .collect();
    cosines.sort_by(|a, b| b.total_cmp(a));
    let angles: Vec<f64> = cosines.iter().map(|c| c.acos()).collect();
    let overlap: f64 = cosines.iter().map(|c| c * c).sum();
    Ok(SubspaceDistance {
        projection: ((k1 + k2) as f64 - 2.0 * overlap).max(0.0).sqrt(),
        max_angle: angles.last().copied().unwrap_or(0.0),
        angles,
    })
}

/// The Gram matrix `[X Y]ᵀ[X Y]` of the columns, `X` the first `k1`,
/// over the rows of `compared`.
fn gram<I: Interrupts>(
    resources: &mut Resources<'_, I>,
    columns: &[Rows<'_>],
    k1: usize,
    compared: ComparedRows,
) -> Result<DMatrix<f64>, SpectralError> {
    let k = columns.len();
    let rows = columns.iter().map(|column| column.id_space()).max().unwrap_or(0) as usize;
    let block = rows.div_ceil(GRAM_BLOCKS);
    let entries = k * k;
    let _partials =
        resources.memory.charge_elements::<f64>(entries.saturating_mul(GRAM_BLOCKS + 2))?;
    let products = |range: std::ops::Range<usize>| {
        let mut sums = vec![0.0; entries];
        let mut row = vec![0.0; k];
        for i in range {
            for (value, column) in row.iter_mut().zip(columns) {
                *value = column.get(i as u64, 0);
            }
            let present = |values: &[f64]| values.iter().any(|&v| v != 0.0);
            if compared == ComparedRows::Shared && !(present(&row[..k1]) && present(&row[k1..])) {
                continue;
            }
            for a in 0..k {
                for b in a..k {
                    sums[a * k + b] += row[a] * row[b];
                }
            }
        }
        sums
    };
    let add = |mut total: Vec<f64>, part: Vec<f64>| {
        for (t, p) in total.iter_mut().zip(part) {
            *t += p;
        }
        total
    };
    let sums = dense::fold(resources, rows, block, vec![0.0; entries], products, add)?;
    Ok(DMatrix::from_fn(k, k, |a, b| sums[a.min(b) * k + a.max(b)]))
}

/// The Cholesky factor `L` of a Gram matrix `G = LLᵀ`.
fn factor(gram: DMatrix<f64>) -> Result<DMatrix<f64>, SpectralError> {
    let cholesky = gram.cholesky().ok_or(SpectralError::RankDeficient)?;
    Ok(cholesky.l())
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests;
