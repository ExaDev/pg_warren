//! Segment encoding: each segment of a graph version as bytes, for the
//! adapter to chunk into `warren.segments` (DESIGN.md §5.1, §5.3).
//!
//! A segment is a 16-byte header followed by its payload, all little-endian:
//!
//! | Bytes | Field |
//! |---|---|
//! | 0..4 | magic `WRSG` |
//! | 4 | format, currently 1 |
//! | 5 | [`SegmentKind`] |
//! | 6 | element width in bytes (0 for variable-width elements) |
//! | 7 | for `id_bytes`, the database encoding (`pg_database.encoding`); for every other kind reserved, 0 |
//! | 8..16 | element count (`u64`); for tombstones, the id space in bits |
//!
//! Decoding never panics on malformed input: it checks the header, the
//! payload length and then the invariants of the decoded structure, and
//! returns a [`GraphError`] for the first violation (DESIGN.md §5.3).

mod csr;
mod id_map;
mod membership;
mod relation;
mod text_id_map;
mod tombstones;

pub use csr::{CsrPart, CsrSegments};
pub use text_id_map::{TextIdPart, TextIdSegments};

use super::{memory, BuildMemory, DenseId, ExternalId, GraphError, IdPair};

/// The length of a segment header in bytes.
pub const SEGMENT_HEADER_LEN: usize = 16;
const MAGIC: [u8; 4] = *b"WRSG";
const FORMAT: u8 = 1;

/// What a segment holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SegmentKind {
    /// CSR `offsets` (`u64`).
    Offsets = 1,
    /// CSR `targets` (`u32` or `u64`).
    Targets = 2,
    /// CSR `weights` (`f64`).
    Weights = 3,
    /// `id_map`: (external id, dense id) pairs sorted by external id.
    IdMap = 4,
    /// `tombstones`: the bitmap as `u64` words.
    Tombstones = 5,
    /// `rel_dict`: relation names, each a `u32` byte length then UTF-8.
    RelationDictionary = 6,
    /// `membership`: each parent node's community in a quotient version
    /// (`u64`, `u64::MAX` for none; DESIGN.md §6.6).
    Membership = 7,
    /// `id_offsets`: where each text id starts in `id_bytes` (`u64`,
    /// `id_space + 1`; DESIGN.md §5.7).
    IdOffsets = 8,
    /// `id_bytes`: every text id's bytes in dense-id order, with the
    /// database encoding in header byte 7.
    IdBytes = 9,
    /// `id_order`: the dense ids sorted by text id (`u32` or `u64`).
    IdOrder = 10,
}

impl SegmentKind {
    /// Checks a segment's header (its first [`SEGMENT_HEADER_LEN`] bytes)
    /// as decoding does, before its payload is read: that it is a segment
    /// of this kind, with elements `width` bytes wide, whose payload is
    /// `payload_len` bytes. Returns the header's count (for tombstones,
    /// the id space in bits, stored in whole `u64` words). An `id_bytes`
    /// header is refused here: [`check_id_bytes_header`] checks it with its
    /// encoding.
    pub fn check_header(
        self,
        header: &[u8],
        width: usize,
        payload_len: u64,
    ) -> Result<u64, GraphError> {
        if self == Self::IdBytes {
            return Err(GraphError::IdBytesEncodingUnchecked);
        }
        checked_header(self, header, width, payload_len).map(|header| header.count)
    }
}

/// Checks an `id_bytes` header (its first [`SEGMENT_HEADER_LEN`] bytes)
/// as decoding does, before its payload is read: its kind, a width of 1, a
/// payload of `payload_len` bytes, and that it records `encoding`, the
/// database's (DESIGN.md §5.7). Returns the header's count, the bytes'
/// length. Whether the bytes are valid in that encoding is not checked
/// here (see DESIGN.md §5.7).
pub fn check_id_bytes_header(
    header: &[u8],
    payload_len: u64,
    encoding: u8,
) -> Result<u64, GraphError> {
    let header = checked_header(SegmentKind::IdBytes, header, 1, payload_len)?;
    match header.encoding == encoding {
        true => Ok(header.count),
        false => Err(GraphError::IdEncoding { expected: encoding, found: header.encoding }),
    }
}

/// [`SegmentKind::check_header`]'s checks, returning the header.
fn checked_header(
    kind: SegmentKind,
    header: &[u8],
    width: usize,
    payload_len: u64,
) -> Result<Header, GraphError> {
    let header = header.get(..SEGMENT_HEADER_LEN).ok_or(GraphError::SegmentTruncated)?;
    let (header, _) = read_header(header, kind)?;
    expect_width(&header, width)?;
    let elements = match kind {
        SegmentKind::Tombstones => header.count.div_ceil(64),
        _ => header.count,
    };
    expect_length(payload_len, elements, width)?;
    Ok(header)
}

/// A decoded segment header.
pub(crate) struct Header {
    pub(crate) width: u8,
    pub(crate) count: u64,
    /// Byte 7: an `id_bytes` segment's encoding, else 0.
    pub(crate) encoding: u8,
}

/// A fixed-width element of a segment payload.
pub(crate) trait Element: Sized {
    /// The encoded width in bytes.
    const WIDTH: usize;
    /// Appends the encoding of `self`.
    fn write(&self, out: &mut Vec<u8>);
    /// Decodes one element from exactly [`Self::WIDTH`] bytes.
    fn read(bytes: &[u8]) -> Self;
}

macro_rules! little_endian_element {
    ($type:ty) => {
        impl Element for $type {
            const WIDTH: usize = std::mem::size_of::<$type>();

            fn write(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }

            fn read(bytes: &[u8]) -> Self {
                let mut array = [0; std::mem::size_of::<$type>()];
                array.copy_from_slice(bytes);
                <$type>::from_le_bytes(array)
            }
        }
    };
}

little_endian_element!(u8);
little_endian_element!(u32);
little_endian_element!(u64);
little_endian_element!(f64);

impl<E: ExternalId> Element for IdPair<E> {
    const WIDTH: usize = E::WIDTH + DenseId::WIDTH;

    fn write(&self, out: &mut Vec<u8>) {
        self.external.write_le(out);
        self.dense.write(out);
    }

    fn read(bytes: &[u8]) -> Self {
        let (external, dense) = bytes.split_at(E::WIDTH);
        IdPair { external: E::read_le(external), dense: DenseId::read(dense) }
    }
}

/// A new segment buffer holding just the header, with room for the payload;
/// `encoding` is header byte 7, 0 for every kind but `id_bytes`.
pub(crate) fn with_header(
    kind: SegmentKind,
    width: usize,
    count: u64,
    payload_len: usize,
    encoding: u8,
) -> Result<Vec<u8>, GraphError> {
    let mut out = memory::with_capacity(SEGMENT_HEADER_LEN + payload_len)?;
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&[FORMAT, kind as u8, width as u8, encoding]);
    out.extend_from_slice(&count.to_le_bytes());
    Ok(out)
}

/// Receives the chunks of an encoded segment, in order.
pub type ChunkSink<'a, X> = dyn FnMut(&[u8]) -> Result<(), X> + 'a;

/// Encodes a segment of `kind` whose header counts `count` and whose payload
/// is `elements`, passing it to `each` in chunks of `chunk_size` bytes (the
/// last one shorter): exactly [`encode`]'s bytes, split every `chunk_size`
/// bytes, through one chunk buffer charged to `memory` rather than the whole
/// segment. Returns the segment's length in bytes.
pub(crate) fn encode_chunked<E: Element, X: From<GraphError>>(
    kind: SegmentKind,
    count: u64,
    elements: &[E],
    chunk_size: usize,
    memory: &BuildMemory,
    each: &mut ChunkSink<'_, X>,
) -> Result<u64, X> {
    encode_chunked_in(kind, 0, count, elements, chunk_size, memory, each)
}

/// [`encode_chunked`] with header byte 7 set to `encoding`.
pub(crate) fn encode_chunked_in<E: Element, X: From<GraphError>>(
    kind: SegmentKind,
    encoding: u8,
    count: u64,
    elements: &[E],
    chunk_size: usize,
    memory: &BuildMemory,
    each: &mut ChunkSink<'_, X>,
) -> Result<u64, X> {
    let chunk_size = chunk_size.max(1);
    let length = SEGMENT_HEADER_LEN + elements.len() * E::WIDTH;
    let capacity = chunk_size.min(length).max(SEGMENT_HEADER_LEN) + E::WIDTH;
    let _charge = memory.charge(capacity as u64)?;
    let mut chunk = with_header(kind, E::WIDTH, count, capacity - SEGMENT_HEADER_LEN, encoding)?;
    let mut full = |chunk: &mut Vec<u8>| -> Result<(), X> {
        while chunk.len() >= chunk_size {
            each(&chunk[..chunk_size])?;
            chunk.drain(..chunk_size);
        }
        Ok(())
    };
    full(&mut chunk)?;
    for element in elements {
        element.write(&mut chunk);
        full(&mut chunk)?;
    }
    if !chunk.is_empty() {
        each(&chunk)?;
    }
    Ok(length as u64)
}

/// Encodes `elements` as a segment of `kind`.
pub(crate) fn encode<E: Element>(kind: SegmentKind, elements: &[E]) -> Result<Vec<u8>, GraphError> {
    encode_in(kind, 0, elements)
}

/// [`encode`] with header byte 7 set to `encoding`.
pub(crate) fn encode_in<E: Element>(
    kind: SegmentKind,
    encoding: u8,
    elements: &[E],
) -> Result<Vec<u8>, GraphError> {
    let (count, payload_len) = (elements.len() as u64, elements.len() * E::WIDTH);
    let mut out = with_header(kind, E::WIDTH, count, payload_len, encoding)?;
    elements.iter().for_each(|element| element.write(&mut out));
    Ok(out)
}

/// Splits a segment of `kind` into its header and payload.
pub(crate) fn read_header(bytes: &[u8], kind: SegmentKind) -> Result<(Header, &[u8]), GraphError> {
    let (header, payload) =
        bytes.split_at_checked(SEGMENT_HEADER_LEN).ok_or(GraphError::SegmentTruncated)?;
    if header[..4] != MAGIC {
        return Err(GraphError::SegmentMagic);
    }
    if header[4] != FORMAT {
        return Err(GraphError::SegmentFormat { found: header[4] });
    }
    if header[5] != kind as u8 {
        return Err(GraphError::SegmentKind { expected: kind as u8, found: header[5] });
    }
    if header[7] != 0 && kind != SegmentKind::IdBytes {
        return Err(GraphError::SegmentReserved { found: header[7] });
    }
    let header = Header { width: header[6], count: u64::read(&header[8..]), encoding: header[7] };
    Ok((header, payload))
}

/// Checks that a header declares elements of `width` bytes.
pub(crate) fn expect_width(header: &Header, width: usize) -> Result<(), GraphError> {
    if usize::from(header.width) == width {
        Ok(())
    } else {
        Err(GraphError::SegmentWidth { expected: width as u8, found: header.width })
    }
}

/// Checks that a payload of `found` bytes holds exactly `count` elements
/// of `width` bytes.
fn expect_length(found: u64, count: u64, width: usize) -> Result<(), GraphError> {
    match count.checked_mul(width as u64) {
        Some(expected) if expected == found => Ok(()),
        expected => {
            Err(GraphError::SegmentLength { expected: expected.unwrap_or(u64::MAX), found })
        }
    }
}

/// Decodes the payload of a header already read, as `E` elements.
pub(crate) fn elements<E: Element>(header: &Header, payload: &[u8]) -> Result<Vec<E>, GraphError> {
    expect_width(header, E::WIDTH)?;
    expect_length(payload.len() as u64, header.count, E::WIDTH)?;
    let mut elements = memory::with_capacity(payload.len() / E::WIDTH)?;
    elements.extend(payload.chunks_exact(E::WIDTH).map(E::read));
    Ok(elements)
}

/// Decodes a segment of `kind` holding `E` elements.
pub(crate) fn decode<E: Element>(kind: SegmentKind, bytes: &[u8]) -> Result<Vec<E>, GraphError> {
    let (header, payload) = read_header(bytes, kind)?;
    elements(&header, payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{
        AnyCsr, Csr, EdgeList, FixedKeys, GraphCsr, IdMap, RelationDictionary, Tombstones,
    };
    use proptest::prelude::*;

    fn csr_example() -> AnyCsr {
        let mut relations = RelationDictionary::new();
        let r = relations.intern("r").unwrap();
        let mut edges = EdgeList::new(true);
        edges.push(r, 0, 2, 1.5).unwrap();
        edges.push(r, 2, 1, 2.5).unwrap();
        let graph = GraphCsr::build(edges, 1, 3, false).unwrap();
        graph.relations()[0].forward().clone()
    }

    #[test]
    fn a_segment_starts_with_its_header() {
        let segments = csr_example().encode().unwrap();
        assert_eq!(&segments.offsets()[..8], b"WRSG\x01\x01\x08\x00");
        assert_eq!(&segments.offsets()[8..16], &4u64.to_le_bytes());
        assert_eq!(&segments.targets()[4..8], b"\x01\x02\x04\x00");
        assert_eq!(segments.offsets().len(), SEGMENT_HEADER_LEN + 4 * 8);
    }

    #[test]
    fn a_csr_round_trips() {
        let csr = csr_example();
        let segments = csr.encode().unwrap();
        let decoded =
            AnyCsr::decode(3, segments.offsets(), segments.targets(), segments.weights()).unwrap();
        assert_eq!(decoded, csr);
    }

    #[test]
    fn decoding_checks_the_header() {
        let segments = csr_example().encode().unwrap();
        let decode_offsets = |bytes: &[u8]| AnyCsr::decode(3, bytes, segments.targets(), None);
        assert_eq!(decode_offsets(&segments.offsets()[..10]), Err(GraphError::SegmentTruncated));
        let mut bad = segments.offsets().to_vec();
        bad[0] = b'X';
        assert_eq!(decode_offsets(&bad), Err(GraphError::SegmentMagic));
        let mut bad = segments.offsets().to_vec();
        bad[4] = 9;
        assert_eq!(decode_offsets(&bad), Err(GraphError::SegmentFormat { found: 9 }));
        assert_eq!(
            decode_offsets(segments.targets()),
            Err(GraphError::SegmentKind {
                expected: SegmentKind::Offsets as u8,
                found: SegmentKind::Targets as u8
            })
        );
        let mut bad = segments.offsets().to_vec();
        bad[6] = 4;
        assert_eq!(decode_offsets(&bad), Err(GraphError::SegmentWidth { expected: 8, found: 4 }));
        let mut bad = segments.offsets().to_vec();
        bad.push(0);
        assert_eq!(
            decode_offsets(&bad),
            Err(GraphError::SegmentLength { expected: 32, found: 33 })
        );
        let mut bad = segments.offsets().to_vec();
        bad[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(decode_offsets(&bad), Err(GraphError::SegmentLength { .. })));
        let mut bad = segments.offsets().to_vec();
        bad[7] = 1;
        assert_eq!(decode_offsets(&bad), Err(GraphError::SegmentReserved { found: 1 }));
    }

    #[test]
    fn a_header_is_checked_before_its_payload_is_read() {
        let segments = csr_example().encode().unwrap();
        let (header, payload) = segments.offsets().split_at(SEGMENT_HEADER_LEN);
        let payload = payload.len() as u64;
        assert_eq!(SegmentKind::Offsets.check_header(header, 8, payload), Ok(4));
        assert_eq!(
            SegmentKind::Targets.check_header(header, 8, payload),
            Err(GraphError::SegmentKind {
                expected: SegmentKind::Targets as u8,
                found: SegmentKind::Offsets as u8
            })
        );
        assert_eq!(
            SegmentKind::Offsets.check_header(header, 4, payload),
            Err(GraphError::SegmentWidth { expected: 4, found: 8 })
        );
        assert_eq!(
            SegmentKind::Offsets.check_header(header, 8, payload + 1),
            Err(GraphError::SegmentLength { expected: 32, found: 33 })
        );
        assert_eq!(
            SegmentKind::Offsets.check_header(&header[..10], 8, payload),
            Err(GraphError::SegmentTruncated)
        );
        // A tombstone header counts bits, stored in whole words.
        let tombstones = Tombstones::new(70).unwrap().encode().unwrap();
        let header = &tombstones[..SEGMENT_HEADER_LEN];
        assert_eq!(SegmentKind::Tombstones.check_header(header, 8, 16), Ok(70));
        assert_eq!(
            SegmentKind::Tombstones.check_header(header, 8, 24),
            Err(GraphError::SegmentLength { expected: 16, found: 24 })
        );
    }

    #[test]
    fn decoding_requires_the_target_width_of_the_id_space() {
        let wide = AnyCsr::U64(Csr::from_parts(3, vec![0, 1, 1, 1], vec![2u64], None).unwrap());
        let segments = wide.encode().unwrap();
        assert_eq!(
            AnyCsr::decode(3, segments.offsets(), segments.targets(), None),
            Err(GraphError::SegmentWidth { expected: 4, found: 8 })
        );
    }

    #[test]
    fn decoding_checks_the_csr_invariants() {
        let segments = csr_example().encode().unwrap();
        let mut bad = segments.offsets().to_vec();
        bad[SEGMENT_HEADER_LEN + 8..SEGMENT_HEADER_LEN + 16].copy_from_slice(&5u64.to_le_bytes());
        assert_eq!(
            AnyCsr::decode(3, &bad, segments.targets(), None),
            Err(GraphError::OffsetsNotMonotone { row: 1 })
        );
    }

    #[test]
    fn decoding_tombstones_rejects_bits_outside_the_id_space() {
        let mut tombstones = Tombstones::new(70).unwrap();
        tombstones.insert(69).unwrap();
        let mut bytes = tombstones.encode().unwrap();
        assert_eq!(Tombstones::decode(&bytes), Ok(tombstones));
        let last = bytes.len() - 1;
        bytes[last] = 0x80;
        assert_eq!(Tombstones::decode(&bytes), Err(GraphError::TombstoneOutsideIdSpace));
    }

    #[test]
    fn decoding_an_id_map_checks_order_permutation_and_tombstones() {
        let id_map = IdMap::new().rebuild(vec![1i64, 2]).unwrap().rebuild(vec![2]).unwrap();
        let bytes = id_map.encode().unwrap();
        let tombstones = id_map.tombstones().clone();
        assert_eq!(
            IdMap::<FixedKeys<i64>>::decode(&bytes, tombstones.clone()).as_ref(),
            Ok(&id_map)
        );
        assert_eq!(
            IdMap::<FixedKeys<i64>>::decode(&bytes, Tombstones::new(3).unwrap()),
            Err(GraphError::TombstonesLength { expected: 2, found: 3 })
        );
        let pair =
            |external: i64, dense: u64| [external.to_le_bytes(), dense.to_le_bytes()].concat();
        let encode = |pairs: &[Vec<u8>]| {
            let mut bytes = bytes[..8].to_vec();
            bytes.extend_from_slice(&(pairs.len() as u64).to_le_bytes());
            pairs.iter().for_each(|p| bytes.extend_from_slice(p));
            bytes
        };
        assert_eq!(
            IdMap::<FixedKeys<i64>>::decode(&encode(&[pair(2, 0), pair(1, 1)]), tombstones.clone()),
            Err(GraphError::IdMapNotSorted)
        );
        assert_eq!(
            IdMap::<FixedKeys<i64>>::decode(&encode(&[pair(1, 1), pair(2, 1)]), tombstones),
            Err(GraphError::IdMapNotPermutation)
        );
        assert!(matches!(
            IdMap::<FixedKeys<i32>>::decode(&bytes, Tombstones::new(2).unwrap()),
            Err(GraphError::SegmentWidth { expected: 12, found: 16 })
        ));
    }

    #[test]
    fn decoding_a_relation_dictionary_checks_names() {
        let mut relations = RelationDictionary::new();
        relations.intern("cites").unwrap();
        relations.intern("authored").unwrap();
        let bytes = relations.encode().unwrap();
        assert_eq!(RelationDictionary::decode(&bytes).as_ref(), Ok(&relations));
        let mut bad = bytes.clone();
        bad[SEGMENT_HEADER_LEN + 4] = 0xff;
        assert_eq!(RelationDictionary::decode(&bad), Err(GraphError::RelationNameNotUtf8));
        let mut duplicate = RelationDictionary::new();
        duplicate.intern("cites").unwrap();
        duplicate.intern("cites2").unwrap();
        let mut bad = duplicate.encode().unwrap();
        bad.truncate(bad.len() - 10);
        bad.extend_from_slice(&5u32.to_le_bytes());
        bad.extend_from_slice(b"cites");
        assert_eq!(RelationDictionary::decode(&bad), Err(GraphError::DuplicateRelationName));
        let mut bad = bytes.clone();
        bad.pop();
        assert_eq!(RelationDictionary::decode(&bad), Err(GraphError::SegmentTruncated));
        let mut bad = bytes.clone();
        bad.push(0);
        assert_eq!(
            RelationDictionary::decode(&bad),
            Err(GraphError::SegmentLength { expected: 21, found: 22 })
        );
        let mut bad = bytes;
        bad[6] = 4;
        assert_eq!(
            RelationDictionary::decode(&bad),
            Err(GraphError::SegmentWidth { expected: 0, found: 4 })
        );
    }

    fn graphs() -> impl Strategy<Value = (u64, bool, Vec<(u64, u64, u8)>)> {
        (1u64..20, any::<bool>()).prop_flat_map(|(id_space, weighted)| {
            let edges = prop::collection::vec((0..id_space, 0..id_space, any::<u8>()), 0..50);
            (Just(id_space), Just(weighted), edges)
        })
    }

    proptest! {
        #[test]
        fn every_segment_round_trips(
            (id_space, weighted, edge_rows) in graphs(),
            builds in prop::collection::vec(prop::collection::vec(any::<[u8; 16]>(), 0..10), 1..4),
            names in prop::collection::vec("\\PC{0,8}", 0..5),
        ) {
            let mut relations = RelationDictionary::new();
            for name in &names {
                relations.intern(name).unwrap();
            }
            let decoded = RelationDictionary::decode(&relations.encode().unwrap());
            prop_assert_eq!(decoded.as_ref(), Ok(&relations));

            let r = relations.intern("edges").unwrap();
            let mut edges = EdgeList::new(weighted);
            for &(source, target, weight) in &edge_rows {
                edges.push(r, source, target, f64::from(weight) / 7.0).unwrap();
            }
            let graph = GraphCsr::build(edges, relations.len(), id_space, true).unwrap();
            for csr in [graph.relations()[0].forward(), graph.relation(r).unwrap().reverse().unwrap()] {
                let segments = csr.encode().unwrap();
                let decoded = AnyCsr::decode(id_space, segments.offsets(), segments.targets(), segments.weights());
                prop_assert_eq!(decoded.as_ref(), Ok(csr));
            }

            let mut id_map = IdMap::new();
            for externals in builds {
                id_map = id_map.rebuild(externals).unwrap();
            }
            let tombstones = Tombstones::decode(&id_map.tombstones().encode().unwrap()).unwrap();
            let decoded = IdMap::<FixedKeys<[u8; 16]>>::decode(&id_map.encode().unwrap(), tombstones);
            prop_assert_eq!(decoded.as_ref(), Ok(&id_map));
        }

        #[test]
        fn malformed_segments_are_errors_not_panics(
            bytes in prop::collection::vec(any::<u8>(), 0..200),
            header in prop::collection::vec(any::<u8>(), 8),
            payload in prop::collection::vec(any::<u8>(), 0..120),
            count in 0u64..20,
            id_space in 0u64..20,
        ) {
            // A plausible header followed by arbitrary bytes reaches the
            // structural checks, not just the header checks.
            let mut framed = b"WRSG\x01".to_vec();
            framed.extend_from_slice(&header[..3]);
            framed.extend_from_slice(&count.to_le_bytes());
            framed.extend_from_slice(&payload);
            for input in [&bytes, &framed] {
                let _ = AnyCsr::decode(id_space, input, input, Some(input));
                let _ = AnyCsr::decode(id_space, csr_example().encode().unwrap().offsets(), input, None);
                let tombstones = Tombstones::decode(input);
                let _ = IdMap::<FixedKeys<i32>>::decode(input, tombstones.clone().unwrap_or_default());
                let _ = IdMap::<FixedKeys<i64>>::decode(input, tombstones.clone().unwrap_or_default());
                let _ = IdMap::<FixedKeys<[u8; 16]>>::decode(input, tombstones.unwrap_or_default());
                let _ = RelationDictionary::decode(input);
            }
        }
    }

    proptest! {
        #[test]
        fn chunked_encoding_is_the_encoding_split_every_chunk_size_bytes(
            ids in prop::collection::btree_set(any::<i64>(), 0..40),
            rows in prop::collection::vec(0u64..30, 0..30),
            chunk_size in 1usize..80,
        ) {
            let memory = BuildMemory::new(1 << 20);
            let chunks_of = |encode: &mut dyn FnMut(&mut ChunkSink<'_, GraphError>) -> Result<u64, GraphError>| {
                let mut chunks = Vec::new();
                let length = encode(&mut |chunk: &[u8]| {
                    chunks.push(chunk.to_vec());
                    Ok(())
                })
                .unwrap();
                (chunks, length)
            };
            let split = |bytes: Vec<u8>| bytes.chunks(chunk_size).map(<[u8]>::to_vec).collect::<Vec<_>>();

            let id_map = IdMap::new().rebuild(ids.into_iter().collect()).unwrap();
            let (chunks, length) = chunks_of(&mut |each| id_map.encode_chunked(chunk_size, &memory, each));
            let whole = id_map.encode().unwrap();
            prop_assert_eq!(length, whole.len() as u64);
            prop_assert_eq!(chunks, split(whole));
            let tombstones = id_map.tombstones();
            let (chunks, _) = chunks_of(&mut |each| tombstones.encode_chunked(chunk_size, &memory, each));
            prop_assert_eq!(chunks, split(tombstones.encode().unwrap()));

            let mut offsets = vec![0];
            offsets.extend(rows.iter().scan(0, |end, &row| { *end += row; Some(*end) }));
            let edges = offsets.last().copied().unwrap_or(0) as usize;
            let weights = (0..edges).map(|edge| edge as f64 / 3.0).collect();
            let targets = rows.iter().flat_map(|&row| 0..row).collect();
            let id_space = 30.max(rows.len() as u64);
            offsets.resize(id_space as usize + 1, edges as u64);
            let csr = AnyCsr::U64(Csr::from_parts(id_space, offsets, targets, Some(weights)).unwrap());
            let whole = csr.encode().unwrap();
            for (part, bytes) in [(CsrPart::Offsets, whole.offsets()), (CsrPart::Targets, whole.targets()), (CsrPart::Weights, whole.weights().unwrap_or_default())] {
                let (chunks, length) = chunks_of(&mut |each| Ok(csr.encode_part(part, chunk_size, &memory, each)?.unwrap_or(0)));
                prop_assert_eq!(length, bytes.len() as u64);
                prop_assert_eq!(chunks, split(bytes.to_vec()));
            }
            prop_assert_eq!(memory.in_use(), 0);
        }
    }
}
