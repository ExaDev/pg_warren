//! Alignment of a vector of one graph to the id space of another, for
//! comparing across graphs (DESIGN.md §8: the shadow-schema pattern). The
//! dense-id mapping comes from [`crate::version::align`]; this moves the
//! rows.

use super::super::SpectralError;
use crate::compute::{AnyVector, ComputeMemory, Element, Vector};
use crate::graph::DenseId;

/// `vector` (of another graph) with row `i` moved to row `mapping[i]` of a
/// vector over `id_space`; rows that nothing maps to are zero.
pub fn align(
    memory: &ComputeMemory,
    vector: &AnyVector,
    mapping: &[DenseId],
    id_space: u64,
) -> Result<AnyVector, SpectralError> {
    if mapping.len() as u64 != vector.id_space() {
        let (expected, found) = (vector.id_space(), mapping.len() as u64);
        return Err(SpectralError::MappingLength { expected, found });
    }
    let (mut seen, _seen) = memory.filled(id_space as usize, false)?;
    for &target in mapping {
        match seen.get_mut(target as usize) {
            None => return Err(SpectralError::MappedOutside { target, id_space }),
            Some(true) => return Err(SpectralError::MappedTwice { target }),
            Some(seen) => *seen = true,
        }
    }
    Ok(match vector {
        AnyVector::F32(vector) => moved(memory, vector, mapping, id_space)?.into(),
        AnyVector::F64(vector) => moved(memory, vector, mapping, id_space)?.into(),
        AnyVector::I64(vector) => moved(memory, vector, mapping, id_space)?.into(),
        AnyVector::Bool(vector) => moved(memory, vector, mapping, id_space)?.into(),
    })
}

/// The rows of `vector` moved by a checked `mapping`.
fn moved<E: Element>(
    memory: &ComputeMemory,
    vector: &Vector<E>,
    mapping: &[DenseId],
    id_space: u64,
) -> Result<Vector<E>, SpectralError> {
    let width = vector.width();
    let mut aligned = Vector::filled(memory, id_space, width, E::default())?;
    let values = aligned.values_mut();
    for (source, &target) in (0..).zip(mapping) {
        let start = target as usize * width;
        values[start..start + width].copy_from_slice(vector.row(source));
    }
    Ok(aligned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute::{Element, Vector};

    fn memory() -> ComputeMemory {
        ComputeMemory::new(1 << 20)
    }

    fn values<E: Element>(vector: &AnyVector) -> Vec<E> {
        E::vector(vector).unwrap().values().to_vec()
    }

    #[test]
    fn rows_move_to_their_mapped_dense_ids() {
        let matrix: AnyVector =
            Vector::from_values(&memory(), 3, 2, vec![1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0])
                .unwrap()
                .into();
        let aligned = align(&memory(), &matrix, &[3, 1, 0], 5).unwrap();
        assert_eq!((aligned.id_space(), aligned.width()), (5, 2));
        assert_eq!(values::<f32>(&aligned), [5.0, 6.0, 3.0, 4.0, 0.0, 0.0, 1.0, 2.0, 0.0, 0.0]);
        let labels: AnyVector = Vector::from_values(&memory(), 2, 1, vec![7i64, 8]).unwrap().into();
        assert_eq!(values::<i64>(&align(&memory(), &labels, &[1, 0], 2).unwrap()), [8, 7]);
    }

    #[test]
    fn the_mapping_must_cover_the_vector_and_be_one_to_one() {
        let vector: AnyVector =
            Vector::from_values(&memory(), 2, 1, vec![1.0f64, 2.0]).unwrap().into();
        assert_eq!(
            align(&memory(), &vector, &[0], 2),
            Err(SpectralError::MappingLength { expected: 2, found: 1 })
        );
        assert_eq!(
            align(&memory(), &vector, &[0, 2], 2),
            Err(SpectralError::MappedOutside { target: 2, id_space: 2 })
        );
        assert_eq!(
            align(&memory(), &vector, &[1, 1], 2),
            Err(SpectralError::MappedTwice { target: 1 })
        );
    }
}
