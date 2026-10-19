//! A vector's bytes as stored in `warren.vector_chunks` (DESIGN.md §5.1):
//! its elements in row-major order, each little-endian (`bool` as one byte,
//! 0 or 1), with no header — the dtype, width and length are columns of
//! `warren.vectors`. Encoding streams the bytes a chunk at a time, and
//! decoding accepts them a chunk at a time, so neither holds a second copy
//! of the vector.

use std::mem::size_of;

use super::{AnyVector, ComputeError, ComputeMemory, Dtype, Element, MemoryCharge, Vector};

/// An element's stored bytes.
trait Stored: Element {
    /// Appends the element's bytes.
    fn write(self, out: &mut Vec<u8>);

    /// The element `bytes` (exactly `size_of::<Self>()` of them) encode, if
    /// they encode one.
    fn read(bytes: &[u8]) -> Option<Self>;
}

macro_rules! little_endian {
    ($type:ty) => {
        impl Stored for $type {
            fn write(self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }

            fn read(bytes: &[u8]) -> Option<Self> {
                Some(<$type>::from_le_bytes(bytes.try_into().ok()?))
            }
        }
    };
}

little_endian!(f32);
little_endian!(f64);
little_endian!(i64);

impl Stored for bool {
    fn write(self, out: &mut Vec<u8>) {
        out.push(u8::from(self));
    }

    fn read(bytes: &[u8]) -> Option<Self> {
        match bytes {
            [0] => Some(false),
            [1] => Some(true),
            _ => None,
        }
    }
}

/// Passes the bytes of `vector` to `write` in chunks of at most
/// `chunk_bytes` bytes (at least one element each), in order.
pub fn encode<E>(
    vector: &AnyVector,
    chunk_bytes: usize,
    write: impl FnMut(&[u8]) -> Result<(), E>,
) -> Result<(), E> {
    match vector {
        AnyVector::F32(vector) => encode_values(vector.values(), chunk_bytes, write),
        AnyVector::F64(vector) => encode_values(vector.values(), chunk_bytes, write),
        AnyVector::I64(vector) => encode_values(vector.values(), chunk_bytes, write),
        AnyVector::Bool(vector) => encode_values(vector.values(), chunk_bytes, write),
    }
}

fn encode_values<T: Stored, E>(
    values: &[T],
    chunk_bytes: usize,
    mut write: impl FnMut(&[u8]) -> Result<(), E>,
) -> Result<(), E> {
    let per_chunk = (chunk_bytes / size_of::<T>()).max(1);
    let mut buffer = Vec::with_capacity(per_chunk * size_of::<T>());
    for chunk in values.chunks(per_chunk) {
        buffer.clear();
        chunk.iter().for_each(|&value| value.write(&mut buffer));
        write(&buffer)?;
    }
    Ok(())
}

/// Rebuilds a vector from its bytes, a chunk at a time. Its buffer is
/// charged to the compute memory budget and reserved up front.
#[derive(Debug)]
pub struct Decoder {
    typed: Typed,
    expected: u64,
    received: u64,
    carry: Vec<u8>,
}

#[derive(Debug)]
enum Typed {
    F32(Values<f32>),
    F64(Values<f64>),
    I64(Values<i64>),
    Bool(Values<bool>),
}

/// The elements decoded so far, in a buffer reserved for all of them.
#[derive(Debug)]
struct Values<T> {
    id_space: u64,
    width: usize,
    values: Vec<T>,
    charge: MemoryCharge,
}

impl<T: Stored> Values<T> {
    fn new(memory: &ComputeMemory, id_space: u64, width: usize) -> Result<Self, ComputeError> {
        let len = Vector::<T>::len(id_space, width)?;
        let charge = memory.charge_elements::<T>(len)?;
        let mut values = Vec::new();
        values
            .try_reserve_exact(len)
            .map_err(|_| ComputeError::OutOfMemory { bytes: charge.bytes() })?;
        Ok(Self { id_space, width, values, charge })
    }

    /// Appends the elements of `bytes`, a whole number of them.
    fn push(&mut self, bytes: &[u8]) -> Option<()> {
        for element in bytes.chunks_exact(size_of::<T>()) {
            self.values.push(T::read(element)?);
        }
        Some(())
    }

    fn finish(self) -> Result<Vector<T>, ComputeError> {
        Vector::charged(self.id_space, self.width, self.values, self.charge)
    }
}

impl Decoder {
    /// A decoder for a vector of `id_space` rows of `width` elements of
    /// `dtype`.
    pub fn new(
        memory: &ComputeMemory,
        dtype: Dtype,
        id_space: u64,
        width: usize,
    ) -> Result<Self, ComputeError> {
        let typed = match dtype {
            Dtype::F32 => Typed::F32(Values::new(memory, id_space, width)?),
            Dtype::F64 => Typed::F64(Values::new(memory, id_space, width)?),
            Dtype::I64 => Typed::I64(Values::new(memory, id_space, width)?),
            Dtype::Bool => Typed::Bool(Values::new(memory, id_space, width)?),
        };
        let element = match dtype {
            Dtype::F32 => size_of::<f32>(),
            Dtype::F64 | Dtype::I64 => size_of::<f64>(),
            Dtype::Bool => size_of::<bool>(),
        };
        let expected = id_space * width as u64 * element as u64;
        Ok(Self { typed, expected, received: 0, carry: Vec::new() })
    }

    /// Decodes the next `bytes` of the vector.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ComputeError> {
        self.received += bytes.len() as u64;
        if self.received > self.expected {
            return Err(self.malformed());
        }
        let element = self.element_size();
        let mut bytes = bytes;
        if !self.carry.is_empty() {
            let take = (element - self.carry.len()).min(bytes.len());
            self.carry.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.carry.len() < element {
                return Ok(());
            }
            let carry = std::mem::take(&mut self.carry);
            self.push_whole(&carry)?;
        }
        let whole = bytes.len() - bytes.len() % element;
        self.push_whole(&bytes[..whole])?;
        self.carry.extend_from_slice(&bytes[whole..]);
        Ok(())
    }

    /// The vector, once every byte has been pushed.
    pub fn finish(self) -> Result<AnyVector, ComputeError> {
        if self.received != self.expected {
            return Err(self.malformed());
        }
        Ok(match self.typed {
            Typed::F32(values) => values.finish()?.into(),
            Typed::F64(values) => values.finish()?.into(),
            Typed::I64(values) => values.finish()?.into(),
            Typed::Bool(values) => values.finish()?.into(),
        })
    }

    fn element_size(&self) -> usize {
        match self.typed {
            Typed::F32(_) => size_of::<f32>(),
            Typed::F64(_) | Typed::I64(_) => size_of::<f64>(),
            Typed::Bool(_) => size_of::<bool>(),
        }
    }

    fn push_whole(&mut self, bytes: &[u8]) -> Result<(), ComputeError> {
        let pushed = match &mut self.typed {
            Typed::F32(values) => values.push(bytes),
            Typed::F64(values) => values.push(bytes),
            Typed::I64(values) => values.push(bytes),
            Typed::Bool(values) => values.push(bytes),
        };
        pushed.ok_or_else(|| self.malformed())
    }

    fn malformed(&self) -> ComputeError {
        ComputeError::VectorBytes { expected: self.expected, found: self.received }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute::{ComputeMemory, Dtype, Vector};

    fn memory() -> ComputeMemory {
        ComputeMemory::new(1 << 20)
    }

    fn chunks(vector: &AnyVector, chunk_bytes: usize) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        encode(vector, chunk_bytes, |chunk| {
            chunks.push(chunk.to_vec());
            Ok::<_, ComputeError>(())
        })
        .unwrap();
        chunks
    }

    fn decoded(
        dtype: Dtype,
        id_space: u64,
        width: usize,
        chunks: &[Vec<u8>],
    ) -> Result<AnyVector, ComputeError> {
        let mut decoder = Decoder::new(&memory(), dtype, id_space, width)?;
        for chunk in chunks {
            decoder.push(chunk)?;
        }
        decoder.finish()
    }

    #[test]
    fn elements_are_little_endian_in_row_major_order() {
        let vector = Vector::from_values(&memory(), 2, 1, vec![1i64, -2]).unwrap().into();
        let bytes: Vec<u8> = chunks(&vector, 1024).concat();
        assert_eq!(bytes, [1, 0, 0, 0, 0, 0, 0, 0, 254, 255, 255, 255, 255, 255, 255, 255]);
        let flags = Vector::from_values(&memory(), 3, 1, vec![true, false, true]).unwrap().into();
        assert_eq!(chunks(&flags, 1024), [vec![1, 0, 1]]);
    }

    #[test]
    fn a_vector_streams_in_chunks_and_round_trips_across_them() {
        let values: Vec<f32> = (0..10).map(|i| i as f32 / 3.0).collect();
        let vector: AnyVector = Vector::from_values(&memory(), 5, 2, values).unwrap().into();
        let streamed = chunks(&vector, 16);
        assert_eq!(streamed.iter().map(Vec::len).collect::<Vec<_>>(), [16, 16, 8]);
        assert_eq!(decoded(Dtype::F32, 5, 2, &streamed).unwrap(), vector);
        let split: Vec<Vec<u8>> = streamed.concat().chunks(3).map(<[u8]>::to_vec).collect();
        assert_eq!(decoded(Dtype::F32, 5, 2, &split).unwrap(), vector);
        for dtype in [Dtype::F64, Dtype::I64, Dtype::Bool] {
            let filled = match dtype {
                Dtype::F64 => {
                    Vector::filled(&memory(), 3, 1, f64::NAN.copysign(-1.0)).unwrap().into()
                }
                Dtype::I64 => Vector::filled(&memory(), 3, 1, i64::MIN).unwrap().into(),
                _ => Vector::filled(&memory(), 3, 1, true).unwrap().into(),
            };
            let back = decoded(dtype, 3, 1, &chunks(&filled, 5)).unwrap();
            assert_eq!(chunks(&back, 64), chunks(&filled, 64), "{dtype}");
        }
    }

    #[test]
    fn the_byte_count_must_match_the_shape() {
        let short = vec![vec![0u8; 15]];
        assert_eq!(
            decoded(Dtype::F64, 2, 1, &short).unwrap_err(),
            ComputeError::VectorBytes { expected: 16, found: 15 }
        );
        let long = vec![vec![0u8; 17]];
        assert_eq!(
            decoded(Dtype::F64, 2, 1, &long).unwrap_err(),
            ComputeError::VectorBytes { expected: 16, found: 17 }
        );
    }

    #[test]
    fn a_boolean_byte_must_be_zero_or_one() {
        assert_eq!(
            decoded(Dtype::Bool, 2, 1, &[vec![1, 2]]).unwrap_err(),
            ComputeError::VectorBytes { expected: 2, found: 2 }
        );
    }

    #[test]
    fn decoding_is_charged_to_the_budget() {
        let small = ComputeMemory::new(8);
        assert!(matches!(
            Decoder::new(&small, Dtype::F64, 2, 1),
            Err(ComputeError::MemoryLimit { requested: 16, .. })
        ));
    }
}
