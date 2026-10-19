//! Writing a vector as numbered chunks of at most the chunk size, and
//! reading it back by decoding its chunks in order (DESIGN.md §5.1).

use warren_core::compute::{encode, AnyVector, ComputeMemory, Decoder};

use super::{Binding, VectorError, VectorRecord, VectorStore};

/// Stores `vector` at `binding`, replacing any vector of that name, in
/// chunks of at most `chunk_size` bytes. Returns its record.
pub(crate) fn write_vector(
    store: &mut impl VectorStore,
    binding: &Binding<'_>,
    vector: &AnyVector,
    chunk_size: usize,
) -> Result<VectorRecord, VectorError> {
    let (dtype, width, length) = (vector.dtype(), vector.width(), vector.id_space());
    let id = store.insert(binding, dtype, width, length)?;
    store.write_chunks(id, binding.name, &mut |sink| encode(vector, chunk_size, &mut *sink))?;
    Ok(VectorRecord {
        vector: id,
        name: binding.name.to_owned(),
        dtype,
        width,
        length,
        created_by: binding.created_by,
    })
}

/// The vector `record` describes: its chunks, which must be numbered
/// `0..n`, decoded in order into a buffer charged to `memory`.
pub(crate) fn read_vector(
    store: &mut impl VectorStore,
    memory: &ComputeMemory,
    record: &VectorRecord,
) -> Result<AnyVector, VectorError> {
    let mut decoder = Decoder::new(memory, record.dtype, record.length, record.width)?;
    let mut expected = 0;
    store.read_chunks(record.vector, &mut |chunk_no, data| {
        if chunk_no != expected {
            return Err(VectorError::ChunkMissing { vector: record.vector, chunk_no: expected });
        }
        expected += 1;
        Ok(decoder.push(data)?)
    })?;
    Ok(decoder.finish()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::Binding;
    use crate::vector::MemoryVectorStore;
    use pgrx::pg_sys;
    use warren_core::compute::{ComputeError, Dtype, Vector};

    fn memory() -> ComputeMemory {
        ComputeMemory::new(1 << 20)
    }

    fn sample() -> AnyVector {
        let values: Vec<f64> = (0..40).map(|i| f64::from(i) / 7.0).collect();
        Vector::from_values(&memory(), 20, 2, values).unwrap().into()
    }

    #[test]
    fn a_vector_is_written_as_numbered_chunks_and_read_back() {
        let mut store = MemoryVectorStore::default();
        let vector = sample();
        let record = write_vector(
            &mut store,
            &Binding::new(1, 7, "emb", pg_sys::Oid::INVALID),
            &vector,
            100,
        )
        .unwrap();
        assert_eq!(
            (record.name.as_str(), record.dtype, record.width, record.length),
            ("emb", Dtype::F64, 2, 20)
        );
        let sizes: Vec<usize> = store.chunks.values().map(Vec::len).collect();
        assert_eq!(sizes, [96, 96, 96, 32]);
        assert_eq!(read_vector(&mut store, &memory(), &record).unwrap(), vector);
        assert_eq!(store.bound(7).unwrap(), [record]);
    }

    #[test]
    fn writing_a_name_again_replaces_the_vector() {
        let mut store = MemoryVectorStore::default();
        let first =
            write_vector(&mut store, &Binding::new(1, 7, "x", pg_sys::Oid::INVALID), &sample(), 64)
                .unwrap();
        let ones = Vector::filled(&memory(), 20, 1, 1i64).unwrap().into();
        let second =
            write_vector(&mut store, &Binding::new(1, 7, "x", pg_sys::Oid::INVALID), &ones, 64)
                .unwrap();
        assert_ne!(first.vector, second.vector);
        assert_eq!(store.existing(&[first.vector, second.vector]).unwrap(), [second.vector]);
        assert_eq!(read_vector(&mut store, &memory(), &second).unwrap(), ones);
    }

    #[test]
    fn a_gap_in_the_chunks_is_an_error() {
        let mut store = MemoryVectorStore::default();
        let record = write_vector(
            &mut store,
            &Binding::new(1, 7, "emb", pg_sys::Oid::INVALID),
            &sample(),
            100,
        )
        .unwrap();
        store.chunks.remove(&(record.vector, 1));
        assert!(matches!(
            read_vector(&mut store, &memory(), &record),
            Err(VectorError::ChunkMissing { chunk_no: 1, .. })
        ));
    }

    #[test]
    fn missing_or_extra_bytes_are_errors() {
        let mut store = MemoryVectorStore::default();
        let record = write_vector(
            &mut store,
            &Binding::new(1, 7, "emb", pg_sys::Oid::INVALID),
            &sample(),
            100,
        )
        .unwrap();
        store.chunks.remove(&(record.vector, 3));
        assert!(matches!(
            read_vector(&mut store, &memory(), &record),
            Err(VectorError::Compute(ComputeError::VectorBytes { .. }))
        ));
    }

    #[test]
    fn reading_is_charged_to_the_budget() {
        let mut store = MemoryVectorStore::default();
        let record = write_vector(
            &mut store,
            &Binding::new(1, 7, "emb", pg_sys::Oid::INVALID),
            &sample(),
            100,
        )
        .unwrap();
        assert!(matches!(
            read_vector(&mut store, &ComputeMemory::new(100), &record),
            Err(VectorError::Compute(ComputeError::MemoryLimit { .. }))
        ));
    }
}
