//! Encoding of a text id map as its `id_bytes`, `id_offsets` and
//! `id_order` segments (DESIGN.md §5.7). A text version has no `id_map`
//! segment.

use super::{
    decode, elements, encode, encode_chunked, encode_chunked_in, encode_in, read_header, ChunkSink,
    SegmentKind, SEGMENT_HEADER_LEN,
};
use crate::graph::{
    BuildCharge, BuildMemory, GraphError, IdMap, IdOrder, TargetWidth, TextKeys, Tombstones,
};

/// One of a text id map's three segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextIdPart {
    /// `id_bytes`.
    IdBytes,
    /// `id_offsets`.
    IdOffsets,
    /// `id_order`.
    IdOrder,
}

/// The encoded segments of a text id map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextIdSegments {
    id_bytes: Vec<u8>,
    id_offsets: Vec<u8>,
    id_order: Vec<u8>,
}

impl TextIdSegments {
    /// The `id_bytes` segment.
    pub fn id_bytes(&self) -> &[u8] {
        &self.id_bytes
    }

    /// The `id_offsets` segment.
    pub fn id_offsets(&self) -> &[u8] {
        &self.id_offsets
    }

    /// The `id_order` segment, `u32` or `u64` wide.
    pub fn id_order(&self) -> &[u8] {
        &self.id_order
    }
}

impl IdMap<TextKeys> {
    /// Encodes the three arrays as segments, recording `encoding`, the
    /// database's, in the `id_bytes` header. The tombstones are a segment
    /// of their own.
    pub fn encode_text(&self, encoding: u8) -> Result<TextIdSegments, GraphError> {
        let keys = self.keys();
        Ok(TextIdSegments {
            id_bytes: encode_in(SegmentKind::IdBytes, encoding, keys.id_bytes())?,
            id_offsets: encode(SegmentKind::IdOffsets, keys.id_offsets())?,
            id_order: match keys.id_order() {
                IdOrder::U32(order) => encode(SegmentKind::IdOrder, order)?,
                IdOrder::U64(order) => encode(SegmentKind::IdOrder, order)?,
            },
        })
    }

    /// One of [`Self::encode_text`]'s segments in chunks of `chunk_size`
    /// bytes, through a chunk buffer charged to `memory`; returns its
    /// length.
    pub fn encode_text_part<X: From<GraphError>>(
        &self,
        part: TextIdPart,
        encoding: u8,
        chunk_size: usize,
        memory: &BuildMemory,
        each: &mut ChunkSink<'_, X>,
    ) -> Result<u64, X> {
        let keys = self.keys();
        let (bytes, offsets) = (keys.id_bytes(), keys.id_offsets());
        match (part, keys.id_order()) {
            (TextIdPart::IdBytes, _) => {
                let kind = SegmentKind::IdBytes;
                encode_chunked_in(
                    kind,
                    encoding,
                    bytes.len() as u64,
                    bytes,
                    chunk_size,
                    memory,
                    each,
                )
            }
            (TextIdPart::IdOffsets, _) => {
                let kind = SegmentKind::IdOffsets;
                encode_chunked(kind, offsets.len() as u64, offsets, chunk_size, memory, each)
            }
            (TextIdPart::IdOrder, IdOrder::U32(order)) => {
                let kind = SegmentKind::IdOrder;
                encode_chunked(kind, order.len() as u64, order, chunk_size, memory, each)
            }
            (TextIdPart::IdOrder, IdOrder::U64(order)) => {
                let kind = SegmentKind::IdOrder;
                encode_chunked(kind, order.len() as u64, order, chunk_size, memory, each)
            }
        }
    }

    /// [`Self::decode_text`] of the three segments and the `tombstones`
    /// segment, charging `memory` before decoding for the decoded arrays,
    /// the bitmap and the order check's scratch; returns the charge for the
    /// id map it holds.
    pub fn decode_text_within(
        id_bytes: &[u8],
        id_offsets: &[u8],
        id_order: &[u8],
        tombstones: &[u8],
        encoding: u8,
        memory: &BuildMemory,
    ) -> Result<(Self, BuildCharge), GraphError> {
        let mut charge = memory.charge(tombstones.len() as u64)?;
        let tombstones = Tombstones::decode(tombstones)?;
        let payload = |segment: &[u8]| segment.len().saturating_sub(SEGMENT_HEADER_LEN) as u64;
        let scratch = Tombstones::bytes_for(tombstones.id_space());
        charge.grow(payload(id_bytes) + payload(id_offsets) + payload(id_order) + scratch)?;
        let id_map = Self::decode_text(id_bytes, id_offsets, id_order, tombstones, encoding)?;
        charge.shrink(charge.bytes().saturating_sub(id_map.bytes()));
        Ok((id_map, charge))
    }

    /// Decodes a text id map from its three segments and its tombstones,
    /// checking that `id_bytes` is in `encoding`, the database's, and
    /// everything [`IdMap::from_text_segments`] checks. `id_order` must have
    /// the width the id space implies.
    pub fn decode_text(
        id_bytes: &[u8],
        id_offsets: &[u8],
        id_order: &[u8],
        tombstones: Tombstones,
        encoding: u8,
    ) -> Result<Self, GraphError> {
        let offsets = decode::<u64>(SegmentKind::IdOffsets, id_offsets)?;
        let (header, payload) = read_header(id_bytes, SegmentKind::IdBytes)?;
        if header.encoding != encoding {
            return Err(GraphError::IdEncoding { expected: encoding, found: header.encoding });
        }
        let bytes = elements::<u8>(&header, payload)?;
        let id_space = offsets.len().saturating_sub(1) as u64;
        let (header, payload) = read_header(id_order, SegmentKind::IdOrder)?;
        let order = match TargetWidth::for_id_space(id_space) {
            TargetWidth::U32 => IdOrder::U32(elements::<u32>(&header, payload)?.into()),
            TargetWidth::U64 => IdOrder::U64(elements::<u64>(&header, payload)?.into()),
        };
        Self::from_text_segments(bytes.into(), offsets.into(), order, tombstones)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{check_encoding, with_header};
    use super::*;
    use crate::graph::{BuildMemory, GraphError, IdMap, TextIds, TextKeys, Tombstones};
    use proptest::prelude::*;

    const UTF8: u8 = 6;

    fn text_map(builds: &[&[&[u8]]]) -> IdMap<TextKeys> {
        builds.iter().fold(IdMap::new(), |id_map, ids| {
            let memory = BuildMemory::unlimited();
            let mut text = TextIds::new(&memory);
            ids.iter().for_each(|id| text.push(id).unwrap());
            id_map.rebuild_within(text).unwrap().0
        })
    }

    fn example() -> IdMap<TextKeys> {
        text_map(&[&[b"b", b"ab", b"c"], &[b"c", b"ab", b""]])
    }

    fn decode(
        segments: &TextIdSegments,
        tombstones: &Tombstones,
    ) -> Result<IdMap<TextKeys>, GraphError> {
        IdMap::decode_text(
            segments.id_bytes(),
            segments.id_offsets(),
            segments.id_order(),
            tombstones.clone(),
            UTF8,
        )
    }

    #[test]
    fn a_text_id_map_round_trips_with_its_encoding_in_id_bytes() {
        let id_map = example();
        let segments = id_map.encode_text(UTF8).unwrap();
        assert_eq!(&segments.id_bytes()[4..8], &[1, 9, 1, UTF8]);
        assert_eq!(&segments.id_offsets()[4..8], &[1, 8, 8, 0]);
        assert_eq!(&segments.id_order()[4..8], &[1, 10, 4, 0]);
        assert_eq!(segments.id_bytes().len(), SEGMENT_HEADER_LEN + 4);
        assert_eq!(segments.id_offsets().len(), SEGMENT_HEADER_LEN + 5 * 8);
        assert_eq!(segments.id_order().len(), SEGMENT_HEADER_LEN + 4 * 4);
        assert_eq!(decode(&segments, id_map.tombstones()).as_ref(), Ok(&id_map));
        let empty = IdMap::<TextKeys>::new();
        let segments = empty.encode_text(UTF8).unwrap();
        assert_eq!(decode(&segments, &Tombstones::default()), Ok(empty));
    }

    #[test]
    fn a_text_id_map_in_another_encoding_is_refused() {
        let id_map = example();
        let segments = id_map.encode_text(0).unwrap();
        assert_eq!(
            decode(&segments, id_map.tombstones()),
            Err(GraphError::IdEncoding { expected: UTF8, found: 0 })
        );
        let header = &segments.id_bytes()[..SEGMENT_HEADER_LEN];
        assert_eq!(check_encoding(header, 0), Ok(()));
        assert_eq!(
            check_encoding(header, UTF8),
            Err(GraphError::IdEncoding { expected: UTF8, found: 0 })
        );
        assert_eq!(SegmentKind::IdBytes.check_header(header, 1, 4), Ok(4));
    }

    #[test]
    fn decoding_rejects_a_non_zero_byte_7_on_every_kind_but_id_bytes() {
        let id_map = example();
        let segments = id_map.encode_text(UTF8).unwrap();
        let tombstones = id_map.tombstones();
        let decode_parts = |bytes: &[u8], offsets: &[u8], order: &[u8]| {
            IdMap::decode_text(bytes, offsets, order, tombstones.clone(), UTF8)
        };
        let mut offsets = segments.id_offsets().to_vec();
        offsets[7] = UTF8;
        assert_eq!(
            decode_parts(segments.id_bytes(), &offsets, segments.id_order()),
            Err(GraphError::SegmentReserved { found: UTF8 })
        );
        let mut order = segments.id_order().to_vec();
        order[7] = 1;
        assert_eq!(
            decode_parts(segments.id_bytes(), segments.id_offsets(), &order),
            Err(GraphError::SegmentReserved { found: 1 })
        );
        assert_eq!(
            decode_parts(segments.id_offsets(), segments.id_offsets(), segments.id_order()),
            Err(GraphError::SegmentKind { expected: 9, found: 8 })
        );
    }

    /// The segments of the three arrays as given, unchecked.
    fn segments(bytes: &[u8], offsets: &[u64], order: &[u32]) -> TextIdSegments {
        let mut id_bytes =
            with_header(SegmentKind::IdBytes, 1, bytes.len() as u64, 0, UTF8).unwrap();
        id_bytes.extend_from_slice(bytes);
        TextIdSegments {
            id_bytes,
            id_offsets: encode(SegmentKind::IdOffsets, offsets).unwrap(),
            id_order: encode(SegmentKind::IdOrder, order).unwrap(),
        }
    }

    #[test]
    fn decoding_checks_the_offsets_and_the_order() {
        let tombstones = Tombstones::new(2).unwrap();
        let check = |bytes: &[u8], offsets: &[u64], order: &[u32]| {
            decode(&segments(bytes, offsets, order), &tombstones).map(|_| ())
        };
        assert_eq!(check(b"ba", &[0, 1, 2], &[1, 0]), Ok(()));
        assert_eq!(check(b"ba", &[0, 1, 2], &[0, 1]), Err(GraphError::IdMapNotSorted));
        assert_eq!(check(b"ba", &[0, 1, 2], &[1, 1]), Err(GraphError::IdMapNotPermutation));
        assert_eq!(
            check(b"ba", &[0, 1, 3], &[1, 0]),
            Err(GraphError::IdOffsetsEnd { expected: 2, found: 3 })
        );
        assert_eq!(
            check(b"ba", &[0, 3, 2], &[1, 0]),
            Err(GraphError::IdOffsetsNotMonotone { dense: 1 })
        );
        let wide = TextIdSegments {
            id_order: encode(SegmentKind::IdOrder, &[1u64, 0]).unwrap(),
            ..segments(b"ba", &[0, 1, 2], &[1, 0])
        };
        assert_eq!(
            decode(&wide, &tombstones),
            Err(GraphError::SegmentWidth { expected: 4, found: 8 })
        );
        assert_eq!(
            decode(&segments(b"ba", &[0, 1, 2], &[1, 0]), &Tombstones::new(3).unwrap()),
            Err(GraphError::TombstonesLength { expected: 2, found: 3 })
        );
    }

    #[test]
    fn decoding_within_a_budget_charges_what_the_id_map_holds() {
        let id_map = example();
        let segments = id_map.encode_text(UTF8).unwrap();
        let tombstones = id_map.tombstones().encode().unwrap();
        let memory = BuildMemory::unlimited();
        let (decoded, charge) = IdMap::decode_text_within(
            segments.id_bytes(),
            segments.id_offsets(),
            segments.id_order(),
            &tombstones,
            UTF8,
            &memory,
        )
        .unwrap();
        assert_eq!(decoded, id_map);
        assert_eq!(charge.bytes(), decoded.bytes());
        let small = BuildMemory::new(16);
        assert!(matches!(
            IdMap::decode_text_within(
                segments.id_bytes(),
                segments.id_offsets(),
                segments.id_order(),
                &tombstones,
                UTF8,
                &small,
            ),
            Err(GraphError::BuildMemoryLimit { .. })
        ));
    }

    fn text_builds() -> impl Strategy<Value = Vec<Vec<Vec<u8>>>> {
        let id = prop::collection::vec(any::<u8>(), 0..6);
        prop::collection::vec(prop::collection::vec(id, 0..20), 1..4)
    }

    proptest! {
        #[test]
        fn text_segments_round_trip_and_chunk(builds in text_builds(), chunk_size in 1usize..40) {
            let builds: Vec<Vec<&[u8]>> = builds.iter().map(|ids| ids.iter().map(Vec::as_slice).collect()).collect();
            let builds: Vec<&[&[u8]]> = builds.iter().map(Vec::as_slice).collect();
            let id_map = text_map(&builds);
            let segments = id_map.encode_text(UTF8).unwrap();
            let decoded = decode(&segments, id_map.tombstones());
            prop_assert_eq!(decoded.as_ref(), Ok(&id_map));
            let memory = BuildMemory::new(1 << 20);
            for (part, whole) in [
                (TextIdPart::IdBytes, segments.id_bytes()),
                (TextIdPart::IdOffsets, segments.id_offsets()),
                (TextIdPart::IdOrder, segments.id_order()),
            ] {
                let mut chunks = Vec::new();
                let length = id_map
                    .encode_text_part(part, UTF8, chunk_size, &memory, &mut |chunk: &[u8]| {
                        chunks.push(chunk.to_vec());
                        Ok::<_, GraphError>(())
                    })
                    .unwrap();
                prop_assert_eq!(length, whole.len() as u64);
                prop_assert_eq!(chunks, whole.chunks(chunk_size).map(<[u8]>::to_vec).collect::<Vec<_>>());
            }
            prop_assert_eq!(memory.in_use(), 0);
        }

        #[test]
        fn malformed_text_segments_are_errors_not_panics(
            bytes in prop::collection::vec(any::<u8>(), 0..60),
            offsets in prop::collection::vec(0u64..80, 0..8),
            order in prop::collection::vec(0u32..10, 0..8),
            encoding in any::<u8>(),
        ) {
            let framed = segments(&bytes, &offsets, &order);
            let tombstones = Tombstones::new(offsets.len().saturating_sub(1) as u64).unwrap();
            let _ = IdMap::decode_text(framed.id_bytes(), framed.id_offsets(), framed.id_order(), tombstones.clone(), encoding);
            let _ = IdMap::decode_text(&bytes, &bytes, &bytes, tombstones, encoding);
        }
    }
}
