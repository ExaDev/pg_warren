//! How a version's id map is stored (DESIGN.md §5.3, §5.7): a fixed-width
//! id type's as one `id_map` segment of pairs, a text id map's as its
//! `id_bytes` (recording the database encoding), `id_offsets` and
//! `id_order`. The tombstones are a segment of their own either way.

use warren_core::graph::{
    BuildCharge, BuildMemory, ExternalId, FixedKeys, IdKeys, IdMap, TextIdPart, TextKeys,
};

use super::{
    read_segment_within, SegmentError, SegmentStore, SegmentWriter, ID_BYTES, ID_MAP, ID_OFFSETS,
    ID_ORDER,
};
use crate::catalog::VersionId;

/// A key storage whose id map has segments.
pub(crate) trait StoredIds: IdKeys + Sized {
    /// Writes the segments of `id_map`, without its tombstones.
    fn write_id_map<S: SegmentStore>(
        id_map: &IdMap<Self>,
        writer: &mut SegmentWriter<'_, S>,
    ) -> Result<(), SegmentError>;

    /// The id map of `version`, decoded and validated with `tombstones`,
    /// its encoded tombstones; its segments, their decoding and what it
    /// holds are charged to `memory`. Returns the charge for what it holds.
    fn read_id_map(
        segments: &mut impl SegmentStore,
        version: VersionId,
        tombstones: &[u8],
        memory: &BuildMemory,
    ) -> Result<(IdMap<Self>, BuildCharge), SegmentError>;
}

impl<E: ExternalId> StoredIds for FixedKeys<E> {
    fn write_id_map<S: SegmentStore>(
        id_map: &IdMap<Self>,
        writer: &mut SegmentWriter<'_, S>,
    ) -> Result<(), SegmentError> {
        writer.chunked_with_memory(ID_MAP, |chunk_size, memory, each| {
            id_map.encode_chunked(chunk_size, memory, each)
        })
    }

    fn read_id_map(
        segments: &mut impl SegmentStore,
        version: VersionId,
        tombstones: &[u8],
        memory: &BuildMemory,
    ) -> Result<(IdMap<Self>, BuildCharge), SegmentError> {
        let (id_map, _id_map_charge) = read_segment_within(segments, version, ID_MAP, memory)?;
        Ok(IdMap::decode_within(&id_map, tombstones, memory)?)
    }
}

impl StoredIds for TextKeys {
    fn write_id_map<S: SegmentStore>(
        id_map: &IdMap<Self>,
        writer: &mut SegmentWriter<'_, S>,
    ) -> Result<(), SegmentError> {
        let encoding = writer.encoding();
        for (name, part) in [
            (ID_BYTES, TextIdPart::IdBytes),
            (ID_OFFSETS, TextIdPart::IdOffsets),
            (ID_ORDER, TextIdPart::IdOrder),
        ] {
            writer.chunked_with_memory(name, |chunk_size, memory, each| {
                id_map.encode_text_part(part, encoding, chunk_size, memory, each)
            })?;
        }
        Ok(())
    }

    fn read_id_map(
        segments: &mut impl SegmentStore,
        version: VersionId,
        tombstones: &[u8],
        memory: &BuildMemory,
    ) -> Result<(IdMap<Self>, BuildCharge), SegmentError> {
        let encoding = segments.encoding();
        let (bytes, _bytes_charge) = read_segment_within(segments, version, ID_BYTES, memory)?;
        let (offsets, _offsets_charge) =
            read_segment_within(segments, version, ID_OFFSETS, memory)?;
        let (order, _order_charge) = read_segment_within(segments, version, ID_ORDER, memory)?;
        let decoded =
            IdMap::decode_text_within(&bytes, &offsets, &order, tombstones, encoding, memory)?;
        Ok(decoded)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use warren_core::graph::{GraphError, TextIds};

    use super::*;
    use crate::segment::{MemorySegmentStore, TOMBSTONES};

    fn text_map(ids: &[&[u8]]) -> IdMap<TextKeys> {
        let mut text = TextIds::new(&BuildMemory::unlimited());
        for id in ids {
            text.push(id).unwrap();
        }
        IdMap::new().rebuild_within(text).unwrap().0
    }

    fn written<K: StoredIds>(id_map: &IdMap<K>, store: &mut MemorySegmentStore) -> Vec<u8> {
        let memory = BuildMemory::unlimited();
        let mut writer = SegmentWriter::new(store, 1, 7, &memory).unwrap();
        K::write_id_map(id_map, &mut writer).unwrap();
        let tombstones = id_map.tombstones().encode().unwrap();
        writer
            .chunked_with_memory(TOMBSTONES, |_, _, each| {
                each(&tombstones)?;
                Ok(tombstones.len() as u64)
            })
            .unwrap();
        tombstones
    }

    #[test]
    fn a_text_id_map_is_three_segments_that_read_back() {
        let id_map = text_map(&[b"b", b"", b"https://example.org/a"]);
        let mut store = MemorySegmentStore::default();
        let tombstones = written(&id_map, &mut store);
        let names: std::collections::BTreeSet<_> =
            store.chunks.keys().map(|(_, name, _)| name.as_str()).collect();
        assert_eq!(names, [ID_BYTES, ID_OFFSETS, ID_ORDER, TOMBSTONES].into());
        let memory = BuildMemory::unlimited();
        let (read, charge) = TextKeys::read_id_map(&mut store, 1, &tombstones, &memory).unwrap();
        assert_eq!(read, id_map);
        assert_eq!(charge.bytes(), id_map.bytes());
    }

    #[test]
    fn a_text_id_map_of_another_encoding_is_refused() {
        let id_map = text_map(&[b"a"]);
        let mut store = MemorySegmentStore { encoding: 8, ..Default::default() };
        let tombstones = written(&id_map, &mut store);
        store.encoding = 6;
        let memory = BuildMemory::unlimited();
        let error = TextKeys::read_id_map(&mut store, 1, &tombstones, &memory).unwrap_err();
        assert!(
            matches!(error, SegmentError::Graph(GraphError::IdEncoding { expected: 6, found: 8 })),
            "{error:?}"
        );
    }

    #[test]
    fn a_fixed_id_map_is_one_segment_that_reads_back() {
        let id_map = IdMap::<FixedKeys<i64>>::new().rebuild(vec![3, 1, 2]).unwrap();
        let mut store = MemorySegmentStore::default();
        let tombstones = written(&id_map, &mut store);
        assert!(store.chunks.keys().any(|(_, name, _)| name == ID_MAP));
        let memory = BuildMemory::unlimited();
        let (read, _) = FixedKeys::read_id_map(&mut store, 1, &tombstones, &memory).unwrap();
        assert_eq!(read, id_map);
    }
}
