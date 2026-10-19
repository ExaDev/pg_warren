//! Float vectors read as `f64` by dense id, zero past their id space.

use crate::compute::{AnyVector, ComputeError, Dtype, Element, Vector};
use crate::graph::DenseId;

/// A width-`d` `f32` or `f64` vector.
#[derive(Debug, Clone, Copy)]
pub(super) enum Rows<'a> {
    F32(&'a Vector<f32>),
    F64(&'a Vector<f64>),
}

impl<'a> Rows<'a> {
    /// `vector`, if its elements are floats.
    pub(super) fn new(vector: &'a AnyVector) -> Result<Self, ComputeError> {
        match vector {
            AnyVector::F32(vector) => Ok(Self::F32(vector)),
            AnyVector::F64(vector) => Ok(Self::F64(vector)),
            other => {
                Err(ComputeError::DtypeMismatch { expected: Dtype::F64, found: other.dtype() })
            }
        }
    }

    pub(super) fn id_space(self) -> u64 {
        match self {
            Self::F32(vector) => vector.id_space(),
            Self::F64(vector) => vector.id_space(),
        }
    }

    pub(super) fn width(self) -> usize {
        match self {
            Self::F32(vector) => vector.width(),
            Self::F64(vector) => vector.width(),
        }
    }

    /// Element `column` of row `row`; 0 past the id space.
    pub(super) fn get(self, row: DenseId, column: usize) -> f64 {
        fn at<E: Element>(vector: &Vector<E>, row: DenseId, column: usize) -> f64 {
            vector.row(row).get(column).map_or(0.0, |value| value.to_f64())
        }
        match self {
            Self::F32(vector) => at(vector, row, column),
            Self::F64(vector) => at(vector, row, column),
        }
    }
}
