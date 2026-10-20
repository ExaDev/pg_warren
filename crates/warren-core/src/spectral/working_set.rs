//! The compute memory an eigenproblem holds at its peak, known before its
//! first step, so that a solve that cannot fit is refused before it runs
//! (DESIGN.md §6.5).
//!
//! The peak comes at the end of a solve: the operator's `f64` work vectors
//! and degrees, the basis, `H` and its Rayleigh–Ritz decomposition, the `k`
//! eigenvectors and the parents of the connected-components pass, all held
//! at once. Any transient buffer of a step (the push-form scratch of a
//! transposed product without reverse CSRs, `9` bytes a row) is smaller
//! than what the end adds. Not counted: a product's row scratch when the
//! relations are merged on the fly (`store_merged := false`), which
//! depends on the longest row, and per-block partial sums.

use std::mem::size_of;

use super::dense::BLOCK;
use super::{Operator, Precision, SpectralError};
use crate::compute::{ComputeError, ComputeMemory};
use crate::graph::DenseId;

/// The smallest basis, unless the dimension is smaller.
const MIN_BASIS: usize = 20;

/// The `f64` vectors an operator application uses: input, output and the
/// transposed product's scratch, and `D^-1/2 · input` when normalised.
const WORK_VECTORS: u64 = 3;

/// The `size × size` matrices a Rayleigh–Ritz decomposition holds: the
/// copy of `H` it decomposes, the eigenvectors, and them reordered.
pub(super) const RITZ_MATRICES: u64 = 3;

/// What an eigenproblem holds, sized by its graph and options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WorkingSet {
    /// The rows of every vector.
    pub(super) id_space: u64,
    /// The active nodes.
    pub(super) dimension: u64,
    /// The matrix.
    pub(super) operator: Operator,
    /// The eigenpairs.
    pub(super) k: usize,
    /// The element type of the basis and the eigenvectors.
    pub(super) precision: Precision,
}

impl WorkingSet {
    /// The number of basis vectors: `max(2k + 1, 20)`, at most the
    /// dimension.
    pub(super) fn basis_size(&self) -> usize {
        let dimension = usize::try_from(self.dimension).unwrap_or(usize::MAX);
        dimension.min(self.k.saturating_mul(2).saturating_add(1).max(MIN_BASIS))
    }

    /// The bytes the operator charges when it is prepared: its work
    /// vectors, the active flags, the degrees and, when normalised,
    /// `d^-1/2`.
    pub(super) fn operator_bytes(&self) -> u64 {
        let f64s = match self.operator.normalized() {
            true => WORK_VECTORS + 1 + 2,
            false => WORK_VECTORS + 1,
        };
        let row = f64s * size_of::<f64>() as u64 + size_of::<bool>() as u64;
        self.id_space.saturating_mul(row)
    }

    /// The bytes the solve charges beyond the operator's, at its peak.
    pub(super) fn solve_bytes(&self) -> u64 {
        let element = self.precision.element_bytes();
        let size = self.basis_size() as u64;
        let blocked_rows = self.id_space.div_ceil(BLOCK as u64).saturating_mul(BLOCK as u64);
        let basis = blocked_rows.saturating_mul(size).saturating_mul(element);
        let matrices = size
            .saturating_mul(size)
            .saturating_mul(1 + RITZ_MATRICES)
            .saturating_add(size)
            .saturating_mul(size_of::<f64>() as u64);
        let eigenvectors = self.id_space.saturating_mul(self.k as u64).saturating_mul(element);
        let parents = self.id_space.saturating_mul(size_of::<DenseId>() as u64);
        [basis, matrices, eigenvectors, parents].into_iter().fold(0, u64::saturating_add)
    }

    /// The bytes the eigenproblem holds at its peak.
    pub(super) fn bytes(&self) -> u64 {
        self.operator_bytes().saturating_add(self.solve_bytes())
    }

    /// Makes sure the solve's bytes fit `memory` beside what it holds now
    /// (the operator's included), reclaiming held memory as a charge
    /// would, and refuses the eigenproblem if they cannot. Nothing stays
    /// charged: the solve charges its buffers as it allocates them, into
    /// the room this found.
    pub(super) fn admit(&self, memory: &ComputeMemory) -> Result<(), SpectralError> {
        match memory.charge(self.solve_bytes()) {
            Ok(_room) => Ok(()),
            Err(ComputeError::MemoryLimit { in_use, limit, .. }) => {
                Err(SpectralError::WorkingSet {
                    needed: self.bytes(),
                    in_use: in_use.saturating_sub(self.operator_bytes()),
                    limit,
                })
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn working_set(operator: Operator, k: usize, precision: Precision) -> WorkingSet {
        WorkingSet { id_space: 1000, dimension: 900, operator, k, precision }
    }

    #[test]
    fn the_basis_holds_twice_k_plus_one_vectors_but_at_least_twenty() {
        let sizes = [1, 9, 10, 30].map(|k| working_set(Operator::Laplacian, k, Precision::F32));
        assert_eq!(sizes.map(|w| w.basis_size()), [20, 20, 21, 61]);
        let small = WorkingSet { dimension: 7, ..sizes[0] };
        assert_eq!(small.basis_size(), 7);
    }

    #[test]
    fn the_operator_holds_four_f64_rows_and_a_flag_or_six_when_normalised() {
        let plain = working_set(Operator::Adjacency, 3, Precision::F32);
        assert_eq!(plain.operator_bytes(), 1000 * (4 * 8 + 1));
        let normalized = working_set(Operator::NormalizedLaplacian, 3, Precision::F32);
        assert_eq!(normalized.operator_bytes(), 1000 * (6 * 8 + 1));
    }

    #[test]
    fn the_solve_holds_the_basis_the_matrices_the_eigenvectors_and_the_parents() {
        // 1000 rows in blocks of 7 make 1001; 20 basis vectors.
        let f32s = working_set(Operator::Adjacency, 3, Precision::F32);
        let (basis, matrices) = (1001 * 20 * 4, (4 * 20 * 20 + 20) * 8);
        assert_eq!(f32s.solve_bytes(), basis + matrices + 3 * 1000 * 4 + 1000 * 8);
        let f64s = WorkingSet { precision: Precision::F64, ..f32s };
        assert_eq!(f64s.solve_bytes(), 2 * basis + matrices + 3 * 1000 * 8 + 1000 * 8);
        assert_eq!(f64s.bytes(), f64s.operator_bytes() + f64s.solve_bytes());
    }

    #[test]
    fn a_solve_that_cannot_fit_is_refused_naming_its_bytes() {
        let set = working_set(Operator::Adjacency, 3, Precision::F32);
        let memory = ComputeMemory::with_held(set.bytes() + 99, 100);
        let _operator = memory.charge(set.operator_bytes()).unwrap();
        let refused = set.admit(&memory).unwrap_err();
        let limit = set.bytes() + 99;
        assert_eq!(refused, SpectralError::WorkingSet { needed: set.bytes(), in_use: 100, limit });
        assert_eq!(memory.in_use(), 100 + set.operator_bytes());
    }

    #[test]
    fn a_solve_that_fits_leaves_nothing_charged() {
        let set = working_set(Operator::Adjacency, 3, Precision::F32);
        let memory = ComputeMemory::with_held(set.bytes() + 100, 100);
        let _operator = memory.charge(set.operator_bytes()).unwrap();
        set.admit(&memory).unwrap();
        assert_eq!(memory.in_use(), 100 + set.operator_bytes());
    }
}
