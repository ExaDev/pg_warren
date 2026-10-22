//! Loading a persisted vector into a region, for the shared cache
//! (DESIGN.md §5.6, §14 item 38): its elements, row-major in native
//! layout, as the region's one array, read in place as a vector. A vector
//! read privately is decoded into a buffer of its own (`vector::load`).

use std::mem::size_of;
use std::sync::{Arc, Weak};

use warren_core::compute::{AnyVector, ComputeError, ComputeMemory, Dtype, Element, Vector};

use super::LoadError;
use crate::cache::region::{typed_mut, Array, InPlace, Region, RegionError, RegionLayout};
use crate::cache::region::{RegionMemory, Unpadded};
use crate::vector::{VectorError, VectorRecord, VectorStore};

/// The region's one array: the vector's elements.
const MISSING: RegionError = RegionError::MissingArray("vector's elements");

/// The layout of the region `record`'s vector loads into, or `None` for a
/// `bool` vector, which is read privately: a region's arrays are read in
/// place, where any bytes must be a valid element, and a `bool` is valid
/// only as 0 or 1.
pub(in crate::cache) fn plan_vector(
    record: &VectorRecord,
) -> Result<Option<RegionLayout>, LoadError> {
    let Some(width) = element_width(record.dtype) else {
        return Ok(None);
    };
    let elements = usize::try_from(record.length)
        .ok()
        .and_then(|rows| rows.checked_mul(record.width))
        .ok_or(RegionError::TooLarge)?;
    Ok(Some(RegionLayout::plan(record.vector, &[(Array::Values, width, elements)])?))
}

/// The bytes of one element of `dtype` in a region, if it has a region.
fn element_width(dtype: Dtype) -> Option<usize> {
    match dtype {
        Dtype::F32 => Some(size_of::<f32>()),
        Dtype::F64 => Some(size_of::<f64>()),
        Dtype::I64 => Some(size_of::<i64>()),
        Dtype::Bool => None,
    }
}

/// Writes `layout`'s header and the chunks of `record`'s vector into
/// `region`, which holds exactly the layout's bytes. The chunks must be
/// numbered `0..n` and hold exactly the vector's elements, as decoding
/// requires.
pub(in crate::cache) fn fill_vector(
    store: &mut impl VectorStore,
    record: &VectorRecord,
    layout: &RegionLayout,
    region: &mut [u8],
) -> Result<(), LoadError> {
    layout.write_header(region);
    let entry = layout.entry(Array::Values).ok_or(MISSING)?;
    let (into, width) = (&mut region[entry.range()], entry.width);
    let expected = into.len() as u64;
    let (mut at, mut next) = (0usize, 0i32);
    store.read_chunks(record.vector, &mut |chunk_no, data| {
        if chunk_no != next {
            return Err(VectorError::ChunkMissing { vector: record.vector, chunk_no: next });
        }
        next += 1;
        let found = at as u64 + data.len() as u64;
        let end = at + data.len();
        let to = into.get_mut(at..end).ok_or(ComputeError::VectorBytes { expected, found })?;
        to.copy_from_slice(data);
        at = end;
        Ok(())
    })?;
    if at as u64 != expected {
        let short = ComputeError::VectorBytes { expected, found: at as u64 };
        return Err(VectorError::from(short).into());
    }
    to_native(into, width)
}

/// Puts stored little-endian elements of `width` bytes in native byte
/// order: nothing on a little-endian target.
fn to_native(bytes: &mut [u8], width: usize) -> Result<(), LoadError> {
    fn swap<T: Unpadded>(bytes: &mut [u8], from_le: fn(T) -> T) -> Result<(), LoadError> {
        typed_mut::<T>(bytes)?.iter_mut().for_each(|value| *value = from_le(*value));
        Ok(())
    }
    match (cfg!(target_endian = "little"), width) {
        (true, _) => Ok(()),
        (false, 4) => swap::<u32>(bytes, u32::from_le),
        (false, _) => swap::<u64>(bytes, u64::from_le),
    }
}

/// The vector `record` describes, which `memory` holds as
/// [`fill_vector`] wrote it, read in place and charged to nothing; and its
/// region, alive while the vector or anything read from it is.
pub(in crate::cache) fn read_vector(
    memory: Box<dyn RegionMemory>,
    record: &VectorRecord,
) -> Result<(AnyVector, Weak<Region>), LoadError> {
    let region = Region::open(memory, record.vector)?;
    let vector = match record.dtype {
        Dtype::F32 => AnyVector::F32(in_place(&region, record)?),
        Dtype::F64 => AnyVector::F64(in_place(&region, record)?),
        Dtype::I64 => AnyVector::I64(in_place(&region, record)?),
        Dtype::Bool => return Err(MISSING.into()),
    };
    Ok((vector, Arc::downgrade(&region)))
}

/// The elements of `record`'s vector in `region`, as a vector of `T`.
fn in_place<T: Element + InPlace>(
    region: &Arc<Region>,
    record: &VectorRecord,
) -> Result<Vector<T>, LoadError> {
    let values = region.view::<T>(Array::Values)?.ok_or(MISSING)?;
    // Shared, so charged nothing; a copy (changing it) is the reader's.
    let unlimited = ComputeMemory::new(u64::MAX);
    Vector::shared(&unlimited, record.length, record.width, values)
        .map_err(|error| VectorError::from(error).into())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::pg_sys;

    use super::*;
    use crate::cache::region::HeapMemory;
    use crate::vector::{read_vector as decode, write_vector, Binding, MemoryVectorStore};

    fn memory() -> ComputeMemory {
        ComputeMemory::new(1 << 20)
    }

    /// `vector`, stored in chunks of 24 bytes, and its record.
    fn stored(vector: &AnyVector) -> (MemoryVectorStore, VectorRecord) {
        let mut store = MemoryVectorStore::default();
        let binding = Binding::new(1, 7, "v", pg_sys::Oid::INVALID);
        let record = write_vector(&mut store, &binding, vector, 24).unwrap();
        (store, record)
    }

    /// `record`'s vector loaded into a heap region, as the shared cache
    /// loads it into a segment.
    fn loaded(
        store: &mut MemoryVectorStore,
        record: &VectorRecord,
    ) -> Result<(AnyVector, Weak<Region>), LoadError> {
        let layout = plan_vector(record)?.expect("a region");
        let mut region = HeapMemory::zeroed(layout.len())?;
        fill_vector(store, record, &layout, region.bytes_mut())?;
        read_vector(Box::new(region), record)
    }

    fn samples() -> Vec<AnyVector> {
        let floats: Vec<f64> = (0..30).map(|i| f64::from(i) / 3.0).collect();
        let singles: Vec<f32> = (0..7).map(|i| i as f32 * 0.5).collect();
        let integers: Vec<i64> = (0..10).map(|i| i64::MAX - i).collect();
        vec![
            Vector::from_values(&memory(), 15, 2, floats).unwrap().into(),
            Vector::from_values(&memory(), 7, 1, singles).unwrap().into(),
            Vector::from_values(&memory(), 10, 1, integers).unwrap().into(),
            Vector::from_values(&memory(), 0, 3, Vec::<f64>::new()).unwrap().into(),
        ]
    }

    #[test]
    fn a_vector_in_a_region_reads_as_it_decodes() {
        for vector in samples() {
            let (mut store, record) = stored(&vector);
            let (shared, region) = loaded(&mut store, &record).unwrap();
            assert_eq!(shared, decode(&mut store, &memory(), &record).unwrap());
            assert_eq!(shared, vector);
            assert_eq!(shared.bytes(), vector.bytes(), "it views exactly its elements");
            assert!(region.strong_count() > 0, "the vector keeps its region");
            drop(shared);
            assert_eq!(region.strong_count(), 0, "and it alone");
        }
    }

    #[test]
    fn a_bool_vector_has_no_region() {
        let flags: AnyVector = Vector::filled(&memory(), 3, 1, true).unwrap().into();
        assert!(plan_vector(&stored(&flags).1).unwrap().is_none());
    }

    #[test]
    fn a_region_is_sized_exactly_for_its_elements() {
        let (_, record) = stored(&samples()[0]);
        let layout = plan_vector(&record).unwrap().unwrap();
        let entry = layout.entry(Array::Values).unwrap();
        assert_eq!((entry.width, entry.elements), (8, 30));
        assert_eq!(layout.len(), entry.range().end);
    }

    #[test]
    fn a_missing_chunk_or_a_wrong_length_is_an_error() {
        let vector = &samples()[0];
        let (mut store, record) = stored(vector);
        store.chunks.remove(&(record.vector, 1));
        assert!(matches!(
            loaded(&mut store, &record),
            Err(LoadError::Vector(VectorError::ChunkMissing { chunk_no: 1, .. }))
        ));
        let (mut store, record) = stored(vector);
        let last = store.chunks.keys().last().copied().unwrap();
        store.chunks.remove(&last);
        assert!(matches!(
            loaded(&mut store, &record),
            Err(LoadError::Vector(VectorError::Compute(ComputeError::VectorBytes { .. })))
        ));
        let (mut store, record) = stored(vector);
        store.chunks.insert((record.vector, last.1 + 1), vec![0; 8]);
        assert!(matches!(
            loaded(&mut store, &record),
            Err(LoadError::Vector(VectorError::Compute(ComputeError::VectorBytes { .. })))
        ));
    }

    #[test]
    fn a_region_of_another_vector_is_refused() {
        let (mut store, record) = stored(&samples()[1]);
        let layout = plan_vector(&record).unwrap().unwrap();
        let mut region = HeapMemory::zeroed(layout.len()).unwrap();
        fill_vector(&mut store, &record, &layout, region.bytes_mut()).unwrap();
        let other = VectorRecord { vector: record.vector + 1, ..record };
        assert!(matches!(
            read_vector(Box::new(region), &other),
            Err(LoadError::Region(RegionError::Version { .. }))
        ));
    }
}
