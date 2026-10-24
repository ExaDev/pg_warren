//! The compute memory an eigenproblem holds at its peak, known before its
//! first step, so that a solve that cannot fit is refused before it runs
//! (DESIGN.md §6.5).
//!
//! The peak comes at the end of a solve: the operator's `f64` work vectors
//! and degrees, the deflation's components and unit vectors when the
//! components' eigenpairs are taken exactly, the basis, `H` and its
//! Rayleigh–Ritz decomposition, and the eigenvectors, all held at once.
//! Without a deflation, the connected-components pass (its parents,
//! colours and flags, at most 11 bytes a row) comes last, beside all `k`
//! eigenvectors, and outweighs any transient buffer of
//! a product (the push-form scratch of a transposed product without
//! reverse CSRs, `9` bytes a row), which comes beside one eigenvector
//! fewer. With one, that scratch, for the last eigenvector's residual, is
//! the end's. Not counted: a product's row scratch when the
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

/// The push-form scratch of a transposed product, per row: an `f64` sum
/// and whether it is present.
const PUSH_SCRATCH: u64 = (size_of::<f64>() + size_of::<bool>()) as u64;

/// The components pass, per row: the union-find parents, the colours and
/// the odd-cycle flags, and whether each component is bipartite (counted
/// per row, as every node may be one).
const COMPONENTS_PASS: u64 = (size_of::<DenseId>() + 3 * size_of::<bool>()) as u64;

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
    /// The components whose eigenpairs are taken exactly, if they are
    /// (DESIGN.md §6.5).
    pub(super) deflated: Option<Deflated>,
    /// The pairs Lanczos computes: `k`, or with a deflation those it does
    /// not give.
    pub(super) wanted: usize,
}

/// The components of a deflated eigenproblem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Deflated {
    /// The connected components.
    pub(super) components: u64,
    /// The eigenvectors they give exactly, which Lanczos runs orthogonal
    /// to.
    pub(super) vectors: u64,
}

impl WorkingSet {
    /// The number of basis vectors: `max(2k + 1, 20)`, at most the
    /// dimension Lanczos runs over (the deflated components' eigenvectors
    /// excluded); none when Lanczos is not needed.
    pub(super) fn basis_size(&self) -> usize {
        if self.wanted == 0 {
            return 0;
        }
        let deflated = self.deflated.map_or(0, |deflated| deflated.vectors);
        let dimension = self.dimension.saturating_sub(deflated);
        let dimension = usize::try_from(dimension).unwrap_or(usize::MAX);
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

    /// The bytes the deflation holds once it is prepared: each node's
    /// component, colour and unit vector entry, and each component's two
    /// coefficients and whether it is bipartite. (The components pass's
    /// odd-cycle flags, a byte a row, are released before the solve.)
    pub(super) fn deflation_bytes(&self) -> u64 {
        let Some(Deflated { components, .. }) = self.deflated else { return 0 };
        let row = (size_of::<DenseId>() + size_of::<bool>() + size_of::<f64>()) as u64;
        let component = (2 * size_of::<f64>() + size_of::<bool>()) as u64;
        self.id_space.saturating_mul(row).saturating_add(components.saturating_mul(component))
    }

    /// The bytes held before the solve starts: the operator's and the
    /// deflation's.
    pub(super) fn prepared_bytes(&self) -> u64 {
        self.operator_bytes().saturating_add(self.deflation_bytes())
    }

    /// The bytes the solve charges beyond the prepared ones, at its peak.
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
        let eigenvectors =
            |count: usize| self.id_space.saturating_mul(count as u64).saturating_mul(element);
        let end =
            match self.deflated {
                None => eigenvectors(self.k)
                    .saturating_add(self.id_space.saturating_mul(COMPONENTS_PASS)),
                Some(_) => eigenvectors(self.k - 1)
                    .saturating_add(self.id_space.saturating_mul(PUSH_SCRATCH)),
            };
        [basis, matrices, end].into_iter().fold(0, u64::saturating_add)
    }

    /// The bytes the eigenproblem holds at its peak.
    pub(super) fn bytes(&self) -> u64 {
        self.prepared_bytes().saturating_add(self.solve_bytes())
    }

    /// Makes sure the solve's bytes fit `memory` beside what it holds now
    /// (the prepared bytes included), reclaiming held memory as a charge
    /// would, and refuses the eigenproblem if they cannot. Nothing stays
    /// charged: the solve charges its buffers as it allocates them, into
    /// the room this found.
    pub(super) fn admit(&self, memory: &ComputeMemory) -> Result<(), SpectralError> {
        match memory.charge(self.solve_bytes()) {
            Ok(_room) => Ok(()),
            Err(ComputeError::MemoryLimit { in_use, limit, .. }) => {
                Err(SpectralError::WorkingSet {
                    needed: self.bytes(),
                    in_use: in_use.saturating_sub(self.prepared_bytes()),
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
        WorkingSet {
            id_space: 1000,
            dimension: 900,
            operator,
            k,
            precision,
            deflated: None,
            wanted: k,
        }
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
        assert_eq!(f32s.solve_bytes(), basis + matrices + 3 * 1000 * 4 + 1000 * 11);
        let f64s = WorkingSet { precision: Precision::F64, ..f32s };
        assert_eq!(f64s.solve_bytes(), 2 * basis + matrices + 3 * 1000 * 8 + 1000 * 11);
        assert_eq!(f64s.bytes(), f64s.operator_bytes() + f64s.solve_bytes());
    }

    #[test]
    fn a_deflated_solve_holds_the_components_and_unit_vectors_and_no_parents() {
        // 900 active nodes in 30 components, k = 40 of which Lanczos
        // computes 10: the basis still has 2k + 1 vectors.
        let set = WorkingSet {
            deflated: Some(Deflated { components: 30, vectors: 30 }),
            wanted: 10,
            ..working_set(Operator::NormalizedLaplacian, 40, Precision::F32)
        };
        assert_eq!(set.basis_size(), 81);
        assert_eq!(set.deflation_bytes(), 1000 * (8 + 1 + 8) + 30 * (16 + 1));
        assert_eq!(set.prepared_bytes(), set.operator_bytes() + set.deflation_bytes());
        let (basis, matrices) = (1001 * 81 * 4, (4 * 81 * 81 + 81) * 8);
        // One eigenvector fewer and a transposed product's push scratch.
        assert_eq!(set.solve_bytes(), basis + matrices + 39 * 1000 * 4 + 1000 * 9);
        // Over 870 + 30 active nodes, at most 870 basis vectors.
        let narrow = WorkingSet { k: 500, wanted: 470, ..set };
        assert_eq!(narrow.basis_size(), 870);
        // When the components give every pair, there is no basis.
        let none = WorkingSet { k: 20, wanted: 0, ..set };
        assert_eq!(none.basis_size(), 0);
        assert_eq!(none.solve_bytes(), 19 * 1000 * 4 + 1000 * 9);
        // The random walk's bipartite components give a second vector each,
        // which Lanczos also runs orthogonal to.
        let walk = Deflated { components: 30, vectors: 60 };
        let walk = WorkingSet { deflated: Some(walk), k: 500, wanted: 440, ..set };
        assert_eq!(walk.basis_size(), 840);
        assert_eq!(walk.deflation_bytes(), set.deflation_bytes());
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
