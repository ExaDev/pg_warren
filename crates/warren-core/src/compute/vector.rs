//! Dense per-node vectors: `id_space` rows of `width` elements, row-major,
//! indexed by dense id; `width > 1` makes an N×d matrix (DESIGN.md §6.2).

use std::mem::size_of;

use super::{ComputeError, ComputeMemory, Dtype, Element, MemoryCharge};
use crate::graph::{DenseId, SegmentBuffer, Tombstones};

/// A dense vector (or, with `width > 1`, a row-major matrix) of `E`,
/// charged to the compute memory budget it was allocated from; a shared
/// vector is charged only once it is changed, which copies it.
#[derive(Debug)]
pub struct Vector<E> {
    id_space: u64,
    width: usize,
    values: SegmentBuffer<E>,
    charge: MemoryCharge,
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
        Ok(Self { id_space, width, values: values.into(), charge })
    }

    /// A vector of `id_space` rows of `width` elements from `values`,
    /// row-major. This adopts a buffer the caller has already allocated, so
    /// unlike [`Vector::filled`] it is charged after the allocation; the
    /// charge covers the buffer's whole capacity.
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
        let charge = memory.charge_elements::<E>(values.capacity())?;
        Ok(Self { id_space, width, values: values.into(), charge })
    }

    /// A vector of `id_space` rows of `width` elements, row-major, read in
    /// place from a shared view or an owned buffer. Nothing is charged
    /// until it is changed ([`Self::values_mut`]), which charges the copy
    /// to `memory`.
    pub fn shared(
        memory: &ComputeMemory,
        id_space: u64,
        width: usize,
        values: SegmentBuffer<E>,
    ) -> Result<Self, ComputeError> {
        let len = Self::len(id_space, width)?;
        if values.len() != len {
            return Err(ComputeError::VectorLength {
                expected: len as u64,
                found: values.len() as u64,
            });
        }
        Ok(Self { id_space, width, values, charge: memory.nothing() })
    }

    /// A vector of `values`, already charged to the budget by `charge`.
    pub(crate) fn charged(
        id_space: u64,
        width: usize,
        values: Vec<E>,
        charge: MemoryCharge,
    ) -> Result<Self, ComputeError> {
        let len = Self::len(id_space, width)?;
        if values.len() != len {
            return Err(ComputeError::VectorLength {
                expected: len as u64,
                found: values.len() as u64,
            });
        }
        Ok(Self { id_space, width, values: values.into(), charge })
    }

    /// The number of elements of `id_space` rows of `width`.
    pub(crate) fn len(id_space: u64, width: usize) -> Result<usize, ComputeError> {
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

    /// The heap bytes its elements hold (a shared vector: the bytes it
    /// views).
    pub fn bytes(&self) -> u64 {
        self.values.bytes()
    }

    /// Every element, row-major.
    pub fn values(&self) -> &[E] {
        &self.values
    }

    /// Every element, row-major, mutably. A shared vector is first copied,
    /// charged to its budget; if that fails, nothing changes.
    pub fn values_mut(&mut self) -> Result<&mut [E], ComputeError> {
        let copy = if self.values.is_shared() { self.bytes() } else { 0 };
        if copy > 0 {
            self.charge.grow(copy)?;
        }
        match self.values.make_mut() {
            Ok(values) => Ok(values),
            Err(_) => {
                self.charge.shrink(copy);
                Err(ComputeError::OutOfMemory { bytes: copy })
            }
        }
    }

    /// Sets every element of each tombstoned row to zero (`false`).
    pub fn clear_tombstoned(&mut self, tombstones: &Tombstones) -> Result<(), ComputeError> {
        let width = self.width;
        for (node, row) in (0..).zip(self.values_mut()?.chunks_mut(width)) {
            if tombstones.contains(node) {
                row.fill(E::default());
            }
        }
        Ok(())
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

    /// Sets every element of each tombstoned row to zero (`false`): what
    /// a vector holds for a node that is no longer in the graph.
    pub fn clear_tombstoned(&mut self, tombstones: &Tombstones) -> Result<(), ComputeError> {
        match self {
            Self::F32(vector) => vector.clear_tombstoned(tombstones),
            Self::F64(vector) => vector.clear_tombstoned(tombstones),
            Self::I64(vector) => vector.clear_tombstoned(tombstones),
            Self::Bool(vector) => vector.clear_tombstoned(tombstones),
        }
    }

    /// The heap bytes its elements hold.
    pub fn bytes(&self) -> u64 {
        match self {
            Self::F32(vector) => vector.bytes(),
            Self::F64(vector) => vector.bytes(),
            Self::I64(vector) => vector.bytes(),
            Self::Bool(vector) => vector.bytes(),
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
    use std::sync::Arc;

    use super::*;
    use crate::graph::{HeapSegment, SharedSegment};

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
    fn a_vector_holds_the_bytes_of_its_elements() {
        let matrix: AnyVector = Vector::filled(&memory(), 3, 2, 1.5f32).unwrap().into();
        assert_eq!(matrix.bytes(), 3 * 2 * 4);
        let flags: AnyVector = Vector::filled(&memory(), 5, 1, true).unwrap().into();
        assert_eq!(flags.bytes(), 5);
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
    fn tombstoned_rows_are_cleared_to_zero() {
        let mut tombstones = Tombstones::new(3).unwrap();
        tombstones.insert(1).unwrap();
        let mut matrix: AnyVector = Vector::filled(&memory(), 3, 2, f64::NAN).unwrap().into();
        matrix.clear_tombstoned(&tombstones).unwrap();
        let values = f64::vector(&matrix).unwrap().values();
        assert!(values[0].is_nan() && values[5].is_nan());
        assert_eq!(values[2..4], [0.0, 0.0]);
        let mut flags: AnyVector = Vector::filled(&memory(), 3, 1, true).unwrap().into();
        flags.clear_tombstoned(&tombstones).unwrap();
        assert_eq!(bool::vector(&flags).unwrap().values(), [true, false, true]);
    }

    #[test]
    fn an_adopted_buffer_is_charged_for_its_capacity() {
        let memory = memory();
        let mut values = Vec::with_capacity(8);
        values.extend([1i64, 2, 3, 4]);
        let vector = Vector::from_values(&memory, 4, 1, values).unwrap();
        assert_eq!(memory.in_use(), 64);
        drop(vector);
        assert_eq!(memory.in_use(), 0);
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

    fn shared<E: Clone + Send + Sync + 'static>(values: &[E]) -> SegmentBuffer<E> {
        SegmentBuffer::shared(HeapSegment::of(values))
    }

    #[test]
    fn a_shared_vector_reads_as_the_owned_one_and_is_not_charged() {
        let memory = memory();
        let owned = Vector::from_values(&memory, 2, 2, vec![1i64, 2, 3, 4]).unwrap();
        let held = memory.in_use();
        let shared = Vector::shared(&memory, 2, 2, shared(&[1i64, 2, 3, 4])).unwrap();
        assert_eq!(memory.in_use(), held);
        assert_eq!(shared, owned);
        assert_eq!(shared.row(1), &[3, 4]);
        assert_eq!(shared.bytes(), 32);
    }

    #[test]
    fn the_shape_of_a_shared_vector_is_checked() {
        assert_eq!(
            Vector::shared(&memory(), 2, 2, shared(&[true; 3])).unwrap_err(),
            ComputeError::VectorLength { expected: 4, found: 3 }
        );
    }

    #[test]
    fn changing_a_shared_vector_copies_it_and_charges_the_copy() {
        let memory = memory();
        let region = HeapSegment::of(&[1.0f64, 2.0, 3.0]);
        let mut vector =
            Vector::shared(&memory, 3, 1, SegmentBuffer::shared(Arc::clone(&region) as _)).unwrap();
        vector.values_mut().unwrap()[0] = 9.0;
        assert_eq!(vector.values(), &[9.0, 2.0, 3.0]);
        assert_eq!(region.elements(), &[1.0, 2.0, 3.0]);
        assert_eq!(memory.in_use(), 24);
        vector.values_mut().unwrap()[1] = 8.0;
        assert_eq!(memory.in_use(), 24, "copied once");
        drop(vector);
        assert_eq!(memory.in_use(), 0);
    }

    #[test]
    fn a_copy_past_the_budget_is_an_error_and_changes_nothing() {
        let memory = ComputeMemory::new(15);
        let mut tombstones = Tombstones::new(2).unwrap();
        tombstones.insert(0).unwrap();
        let mut vector: AnyVector =
            Vector::shared(&memory, 2, 1, shared(&[1.0f64, 2.0])).unwrap().into();
        assert!(matches!(
            vector.clear_tombstoned(&tombstones),
            Err(ComputeError::MemoryLimit { requested: 16, .. })
        ));
        assert_eq!(f64::vector(&vector).unwrap().values(), &[1.0, 2.0]);
        assert_eq!(memory.in_use(), 0);
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
