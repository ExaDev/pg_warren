//! Dense per-node vectors: `id_space` rows of `width` elements, row-major,
//! indexed by dense id; `width > 1` makes an N×d matrix (DESIGN.md §6.2).

use std::mem::size_of;

use super::{ComputeError, ComputeMemory, Dtype, Element, MemoryCharge};
use crate::graph::DenseId;

/// A dense vector (or, with `width > 1`, a row-major matrix) of `E`,
/// charged to the compute memory budget it was allocated from.
#[derive(Debug)]
pub struct Vector<E> {
    id_space: u64,
    width: usize,
    values: Vec<E>,
    _charge: MemoryCharge,
}

impl<E: Element> Vector<E> {
    /// A vector of `id_space` rows of `width` copies of `value`.
    pub fn filled(
        memory: &ComputeMemory,
        id_space: u64,
        width: usize,
        value: E,
    ) -> Result<Self, ComputeError> {
        let (values, charge) = memory.filled(Self::len(id_space, width)?, value)?;
        Ok(Self { id_space, width, values, _charge: charge })
    }

    /// A vector of `id_space` rows of `width` elements from `values`,
    /// row-major.
    pub fn from_values(
        memory: &ComputeMemory,
        id_space: u64,
        width: usize,
        values: Vec<E>,
    ) -> Result<Self, ComputeError> {
        let len = Self::len(id_space, width)?;
        if values.len() != len {
            return Err(ComputeError::VectorLength {
                expected: len as u64,
                found: values.len() as u64,
            });
        }
        let charge = memory.charge_elements::<E>(len)?;
        Ok(Self { id_space, width, values, _charge: charge })
    }

    /// The number of elements of `id_space` rows of `width`.
    fn len(id_space: u64, width: usize) -> Result<usize, ComputeError> {
        if width == 0 {
            return Err(ComputeError::ZeroWidth);
        }
        id_space
            .checked_mul(width as u64)
            .and_then(|len| usize::try_from(len).ok())
            .filter(|&len| len.checked_mul(size_of::<E>()).is_some())
            .ok_or(ComputeError::OutOfMemory { bytes: u64::MAX })
    }

    /// The number of rows: the id space of the graph version.
    pub fn id_space(&self) -> u64 {
        self.id_space
    }

    /// The number of elements per row.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Every element, row-major.
    pub fn values(&self) -> &[E] {
        &self.values
    }

    /// Every element, row-major, mutably.
    pub fn values_mut(&mut self) -> &mut [E] {
        &mut self.values
    }

    /// The row of `node`; empty outside the id space.
    pub fn row(&self, node: DenseId) -> &[E] {
        if node >= self.id_space {
            return &[];
        }
        let start = node as usize * self.width;
        &self.values[start..start + self.width]
    }
}

impl<E: PartialEq> PartialEq for Vector<E> {
    fn eq(&self, other: &Self) -> bool {
        self.id_space == other.id_space && self.width == other.width && self.values == other.values
    }
}

/// A vector of any element type.
#[derive(Debug, PartialEq)]
pub enum AnyVector {
    /// `f32` elements.
    F32(Vector<f32>),
    /// `f64` elements.
    F64(Vector<f64>),
    /// `i64` elements.
    I64(Vector<i64>),
    /// `bool` elements.
    Bool(Vector<bool>),
}

impl AnyVector {
    /// The element type.
    pub fn dtype(&self) -> Dtype {
        match self {
            Self::F32(_) => Dtype::F32,
            Self::F64(_) => Dtype::F64,
            Self::I64(_) => Dtype::I64,
            Self::Bool(_) => Dtype::Bool,
        }
    }

    /// The number of rows.
    pub fn id_space(&self) -> u64 {
        match self {
            Self::F32(vector) => vector.id_space(),
            Self::F64(vector) => vector.id_space(),
            Self::I64(vector) => vector.id_space(),
            Self::Bool(vector) => vector.id_space(),
        }
    }

    /// The number of elements per row.
    pub fn width(&self) -> usize {
        match self {
            Self::F32(vector) => vector.width(),
            Self::F64(vector) => vector.width(),
            Self::I64(vector) => vector.width(),
            Self::Bool(vector) => vector.width(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> ComputeMemory {
        ComputeMemory::new(1 << 20)
    }

    #[test]
    fn a_filled_matrix_has_id_space_rows_of_width_elements() {
        let memory = memory();
        let matrix = Vector::filled(&memory, 3, 2, 1.5f64).unwrap();
        assert_eq!((matrix.id_space(), matrix.width()), (3, 2));
        assert_eq!(matrix.values(), &[1.5; 6]);
        assert_eq!(matrix.row(1), &[1.5, 1.5]);
        assert!(matrix.row(3).is_empty());
        assert_eq!(memory.in_use(), 48);
        drop(matrix);
        assert_eq!(memory.in_use(), 0);
    }

    #[test]
    fn rows_are_row_major() {
        let vector = Vector::from_values(&memory(), 2, 2, vec![1i64, 2, 3, 4]).unwrap();
        assert_eq!(vector.row(0), &[1, 2]);
        assert_eq!(vector.row(1), &[3, 4]);
        let any = AnyVector::from(vector);
        assert_eq!((any.dtype(), any.id_space(), any.width()), (Dtype::I64, 2, 2));
        assert_eq!(i64::vector(&any).map(Vector::values), Some(&[1, 2, 3, 4][..]));
        assert!(f64::vector(&any).is_none());
    }

    #[test]
    fn the_shape_is_checked() {
        let memory = memory();
        assert_eq!(Vector::filled(&memory, 3, 0, 0.0f32).unwrap_err(), ComputeError::ZeroWidth);
        assert_eq!(
            Vector::from_values(&memory, 2, 2, vec![true; 3]).unwrap_err(),
            ComputeError::VectorLength { expected: 4, found: 3 }
        );
    }

    #[test]
    fn a_vector_past_the_budget_is_an_error_and_charges_nothing() {
        let memory = ComputeMemory::new(15);
        assert!(matches!(
            Vector::filled(&memory, 2, 1, 0.0f64),
            Err(ComputeError::MemoryLimit { requested: 16, .. })
        ));
        assert_eq!(memory.in_use(), 0);
        let huge = Vector::filled(&ComputeMemory::new(u64::MAX), u64::MAX, 2, 0u8 as i64);
        assert!(matches!(huge, Err(ComputeError::OutOfMemory { .. })));
    }
}
