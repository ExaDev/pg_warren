//! Chunked persistence: a segment's bytes are stored as `warren.segments`
//! rows of at most [`CHUNK_SIZE`] bytes, numbered from 0, and read back by
//! concatenating the chunks in order (DESIGN.md §5.1).

use super::error::BuildError;

/// A version id: `warren.versions.version_id`.
pub(crate) type VersionId = i64;

/// The port to the chunks of each segment of each version.
pub(crate) trait SegmentStore {
    /// Stores one chunk of a segment.
    fn write_chunk(
        &mut self,
        version: VersionId,
        segment: &str,
        chunk_no: i32,
        data: &[u8],
    ) -> Result<(), BuildError>;

    /// The chunks of a segment as (chunk number, bytes), in chunk order.
    fn read_chunks(
        &mut self,
        version: VersionId,
        segment: &str,
    ) -> Result<Vec<(i32, Vec<u8>)>, BuildError>;
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
) -> Result<u64, BuildError> {
    for (chunk_no, chunk) in bytes.chunks(chunk_size).enumerate() {
        let chunk_no = i32::try_from(chunk_no)
            .map_err(|_| BuildError::TooManyChunks { segment: segment.to_owned() })?;
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
) -> Result<Vec<u8>, BuildError> {
    let chunks = store.read_chunks(version, segment)?;
    if chunks.is_empty() {
        return Err(BuildError::SegmentMissing { version, segment: segment.to_owned() });
    }
    let mut bytes = Vec::new();
    for (expected, (chunk_no, data)) in (0..).zip(chunks) {
        if chunk_no != expected {
            return Err(BuildError::ChunkMissing {
                version,
                segment: segment.to_owned(),
                chunk_no: expected,
            });
        }
        bytes.extend_from_slice(&data);
    }
    Ok(bytes)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// An in-memory [`SegmentStore`].
    #[derive(Debug, Default)]
    pub(in crate::build) struct MemorySegmentStore {
        pub(in crate::build) chunks: BTreeMap<(VersionId, String, i32), Vec<u8>>,
    }

    impl SegmentStore for MemorySegmentStore {
        fn write_chunk(
            &mut self,
            version: VersionId,
            segment: &str,
            chunk_no: i32,
            data: &[u8],
        ) -> Result<(), BuildError> {
            self.chunks.insert((version, segment.to_owned(), chunk_no), data.to_vec());
            Ok(())
        }

        fn read_chunks(
            &mut self,
            version: VersionId,
            segment: &str,
        ) -> Result<Vec<(i32, Vec<u8>)>, BuildError> {
            Ok(self
                .chunks
                .iter()
                .filter(|((v, s, _), _)| *v == version && s == segment)
                .map(|((_, _, chunk_no), data)| (*chunk_no, data.clone()))
                .collect())
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
    fn a_segment_without_chunks_is_missing() {
        let mut store = MemorySegmentStore::default();
        assert!(matches!(
            read_segment(&mut store, 3, "id_map"),
            Err(BuildError::SegmentMissing { version: 3, .. })
        ));
    }

    #[test]
    fn a_gap_in_the_chunk_numbers_is_an_error() {
        let mut store = MemorySegmentStore::default();
        write_segment(&mut store, 3, "id_map", &[1, 2, 3], 1).unwrap();
        store.chunks.remove(&(3, "id_map".to_owned(), 1));
        assert!(matches!(
            read_segment(&mut store, 3, "id_map"),
            Err(BuildError::ChunkMissing { chunk_no: 1, .. })
        ));
    }

    #[test]
    fn the_default_chunk_size_is_8_mib() {
        assert_eq!(DEFAULT_CHUNK_SIZE, 8 * 1024 * 1024);
    }
}
