//! Value masks with GraphBLAS semantics: which rows (receivers) a kernel
//! writes (DESIGN.md §6.3, §6.4).

use super::{AnyVector, ComputeError, Element};
use crate::graph::DenseId;

/// A value mask (GraphBLAS semantics): row `i` is selected when the mask's
/// value there is non-zero (`NaN` included), or, with `complement`, when it
/// is zero.
#[derive(Debug, Clone, Copy)]
pub struct Mask<'a> {
    /// A width-1 vector of any element type.
    pub vector: &'a AnyVector,
    /// Select the rows whose mask value is zero instead.
    pub complement: bool,
}

impl Mask<'_> {
    /// Checks that the mask is a width-1 vector over `id_space`.
    pub(crate) fn check(&self, id_space: u64) -> Result<(), ComputeError> {
        let vector = self.vector;
        if vector.id_space() != id_space {
            return Err(ComputeError::IdSpaceMismatch {
                expected: id_space,
                found: vector.id_space(),
            });
        }
        if vector.width() != 1 {
            return Err(ComputeError::WidthMismatch { expected: 1, found: vector.width() });
        }
        Ok(())
    }

    /// Whether the mask selects `row`, which must be in its id space.
    pub(crate) fn selects(&self, row: DenseId) -> bool {
        let row = row as usize;
        let set = match self.vector {
            AnyVector::F32(mask) => mask.values()[row].is_nonzero(),
            AnyVector::F64(mask) => mask.values()[row].is_nonzero(),
            AnyVector::I64(mask) => mask.values()[row].is_nonzero(),
            AnyVector::Bool(mask) => mask.values()[row].is_nonzero(),
        };
        set != self.complement
    }
}
