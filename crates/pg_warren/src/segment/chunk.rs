//! Chunked persistence: a segment's bytes are stored as `warren.segments`
//! rows of at most [`CHUNK_SIZE`] bytes, numbered from 0, and read back by
//! concatenating the chunks in order (DESIGN.md §5.1).

use warren_core::graph::{BuildCharge, BuildMemory};

use super::SegmentError;
use crate::catalog::VersionId;

/// Receives chunks as (chunk number, bytes), one at a time, in chunk order;
/// an error `E` stops the read.
pub(crate) type EachChunk<'a, E> = dyn FnMut(i32, &[u8]) -> Result<(), E> + 'a;

/// The port to the chunks of each segment of each version.
pub(crate) trait SegmentStore {
    /// Stores one chunk of a segment.
    fn write_chunk(
        &mut self,
        version: VersionId,
        segment: &str,
        chunk_no: i32,
        data: &[u8],
    ) -> Result<(), SegmentError>;

    /// The total bytes of a segment's chunks, or `None` if it has none.
    fn segment_bytes(
        &mut self,
        version: VersionId,
        segment: &str,
    ) -> Result<Option<u64>, SegmentError>;

    /// Passes each chunk of a segment to `each` as (chunk number, bytes),
    /// in chunk order.
    fn read_chunks(
        &mut self,
        version: VersionId,
        segment: &str,
        each: &mut EachChunk<'_, SegmentError>,
    ) -> Result<(), SegmentError>;

    /// The database encoding (`pg_database.encoding`), which a text id
    /// map's `id_bytes` header records and must match (DESIGN.md §5.7).
    fn encoding(&self) -> u8;
}

/// The chunk size of production builds: 8 MiB (DESIGN.md §5.1, §14 item 3).
pub(crate) const DEFAULT_CHUNK_SIZE: usize = 8 * 1024 * 1024;

/// The chunk size builds use. In-database test builds (`pg_test`) use 1 KiB,
/// so that their small graphs still span several chunks.
pub(crate) const CHUNK_SIZE: usize =
    if cfg!(feature = "pg_test") { 1024 } else { DEFAULT_CHUNK_SIZE };

/// Stores `bytes` as the chunks of `segment`, each at most `chunk_size`
/// bytes. Returns the number of bytes written.
pub(crate) fn write_segment(
    store: &mut impl SegmentStore,
    version: VersionId,
    segment: &str,
    bytes: &[u8],
    chunk_size: usize,
) -> Result<u64, SegmentError> {
    for (chunk_no, chunk) in bytes.chunks(chunk_size).enumerate() {
        let chunk_no = i32::try_from(chunk_no)
            .map_err(|_| SegmentError::TooManyChunks { segment: segment.to_owned() })?;
        store.write_chunk(version, segment, chunk_no, chunk)?;
    }
    Ok(bytes.len() as u64)
}

/// The bytes of `segment`: its chunks, which must be numbered `0..n`,
/// concatenated in order.
pub(crate) fn read_segment(
    store: &mut impl SegmentStore,
    version: VersionId,
    segment: &str,
) -> Result<Vec<u8>, SegmentError> {
    let memory = BuildMemory::unlimited();
    read_segment_within(store, version, segment, &memory).map(|(bytes, _)| bytes)
}

/// [`read_segment`], charging its bytes to `memory` before they are
/// reserved; returns the charge with them.
pub(crate) fn read_segment_within(
    store: &mut impl SegmentStore,
    version: VersionId,
    segment: &str,
    memory: &BuildMemory,
) -> Result<(Vec<u8>, BuildCharge), SegmentError> {
    let Some(length) = store.segment_bytes(version, segment)? else {
        return Err(SegmentError::SegmentMissing { version, segment: segment.to_owned() });
    };
    let charge = memory.charge(length)?;
    let mut bytes = Vec::new();
    reserve(&mut bytes, usize::try_from(length).unwrap_or(usize::MAX), segment)?;
    let mut expected = 0;
    store.read_chunks(version, segment, &mut |chunk_no, data| {
        if chunk_no != expected {
            return Err(SegmentError::ChunkMissing {
                version,
                segment: segment.to_owned(),
                chunk_no: expected,
            });
        }
        expected += 1;
        reserve(&mut bytes, data.len(), segment)?;
        bytes.extend_from_slice(data);
        Ok(())
    })?;
    Ok((bytes, charge))
}

/// Makes room for `more` bytes in `bytes`, exactly, or fails with
/// `OutOfMemory` (53200) rather than aborting the backend.
fn reserve(bytes: &mut Vec<u8>, more: usize, segment: &str) -> Result<(), SegmentError> {
    bytes
        .try_reserve_exact(more)
        .map_err(|_| SegmentError::OutOfMemory { segment: segment.to_owned(), bytes: more })
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
pub(crate) mod tests {
    use super::*;
    use crate::access::SqlError;
    use pgrx::PgSqlErrorCode;
    use std::collections::BTreeMap;

    #[test]
    fn a_failed_reservation_reports_its_bytes_and_the_segment() {
        let error = reserve(&mut Vec::new(), usize::MAX, "targets:0").unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("out of memory reading segment \"targets:0\": {} bytes", usize::MAX)
        );
        assert_eq!(error.sqlstate(), PgSqlErrorCode::ERRCODE_OUT_OF_MEMORY);
    }

    /// An in-memory [`SegmentStore`], in a database of encoding
    /// `encoding` (UTF8, 6, by default).
    #[derive(Debug)]
    pub(crate) struct MemorySegmentStore {
        pub(crate) chunks: BTreeMap<(VersionId, String, i32), Vec<u8>>,
        pub(crate) encoding: u8,
    }

    /// `PG_UTF8`.
    pub(crate) const UTF8: u8 = 6;

    impl Default for MemorySegmentStore {
        fn default() -> Self {
            Self { chunks: BTreeMap::new(), encoding: UTF8 }
        }
    }

    impl SegmentStore for MemorySegmentStore {
        fn write_chunk(
            &mut self,
            version: VersionId,
            segment: &str,
            chunk_no: i32,
            data: &[u8],
        ) -> Result<(), SegmentError> {
            self.chunks.insert((version, segment.to_owned(), chunk_no), data.to_vec());
            Ok(())
        }

        fn segment_bytes(
            &mut self,
            version: VersionId,
            segment: &str,
        ) -> Result<Option<u64>, SegmentError> {
            let mut chunks =
                self.chunks.iter().filter(|((v, s, _), _)| *v == version && s == segment);
            Ok(chunks.next().map(|(_, first)| {
                chunks.fold(first.len() as u64, |total, (_, data)| total + data.len() as u64)
            }))
        }

        fn read_chunks(
            &mut self,
            version: VersionId,
            segment: &str,
            each: &mut EachChunk<'_, SegmentError>,
        ) -> Result<(), SegmentError> {
            self.chunks
                .iter()
                .filter(|((v, s, _), _)| *v == version && s == segment)
                .try_for_each(|((_, _, chunk_no), data)| each(*chunk_no, data))
        }

        fn encoding(&self) -> u8 {
            self.encoding
        }
    }

    fn chunk_sizes(store: &MemorySegmentStore, version: VersionId) -> Vec<(i32, usize)> {
        store
            .chunks
            .iter()
            .filter(|((v, _, _), _)| *v == version)
            .map(|((_, _, chunk_no), data)| (*chunk_no, data.len()))
            .collect()
    }

    #[test]
    fn a_segment_is_split_into_numbered_chunks_of_at_most_the_chunk_size() {
        let mut store = MemorySegmentStore::default();
        let bytes: Vec<u8> = (0..10).collect();
        let written = write_segment(&mut store, 7, "targets:0", &bytes, 4).unwrap();
        assert_eq!(written, 10);
        assert_eq!(chunk_sizes(&store, 7), [(0, 4), (1, 4), (2, 2)]);
    }

    #[test]
    fn a_segment_smaller_than_a_chunk_is_one_chunk() {
        let mut store = MemorySegmentStore::default();
        write_segment(&mut store, 7, "id_map", &[1, 2, 3], CHUNK_SIZE).unwrap();
        assert_eq!(chunk_sizes(&store, 7), [(0, 3)]);
    }

    #[test]
    fn reading_a_segment_concatenates_its_chunks_in_order() {
        let mut store = MemorySegmentStore::default();
        let bytes: Vec<u8> = (0..=255).collect();
        write_segment(&mut store, 3, "offsets:0", &bytes, 100).unwrap();
        write_segment(&mut store, 4, "offsets:0", &[9], 100).unwrap();
        assert_eq!(read_segment(&mut store, 3, "offsets:0").unwrap(), bytes);
    }

    #[test]
    fn reading_a_segment_reserves_exactly_its_bytes() {
        let mut store = MemorySegmentStore::default();
        let bytes: Vec<u8> = (0..3037).map(|i| (i % 251) as u8).collect();
        write_segment(&mut store, 3, "targets:0", &bytes, 1000).unwrap();
        let read = read_segment(&mut store, 3, "targets:0").unwrap();
        assert_eq!(read, bytes);
        assert_eq!(read.capacity(), read.len());
    }

    #[test]
    fn a_segment_without_chunks_is_missing() {
        let mut store = MemorySegmentStore::default();
        assert!(matches!(
            read_segment(&mut store, 3, "id_map"),
            Err(SegmentError::SegmentMissing { version: 3, .. })
        ));
    }

    #[test]
    fn a_gap_in_the_chunk_numbers_is_an_error() {
        let mut store = MemorySegmentStore::default();
        write_segment(&mut store, 3, "id_map", &[1, 2, 3], 1).unwrap();
        store.chunks.remove(&(3, "id_map".to_owned(), 1));
        assert!(matches!(
            read_segment(&mut store, 3, "id_map"),
            Err(SegmentError::ChunkMissing { chunk_no: 1, .. })
        ));
    }

    #[test]
    fn the_default_chunk_size_is_8_mib() {
        assert_eq!(DEFAULT_CHUNK_SIZE, 8 * 1024 * 1024);
    }
}
