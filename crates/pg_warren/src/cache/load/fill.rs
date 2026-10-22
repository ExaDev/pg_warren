//! Filling a version's region: each segment's chunks copied straight into
//! place after its header is checked, the id map's second direction built
//! in place (DESIGN.md §5.6).

use std::mem::size_of;

use warren_core::graph::{DenseId, GraphError, IdPair, SEGMENT_HEADER_LEN};

use super::plan::{Placed, Plan};
use super::LoadError;
use crate::cache::region::{typed, typed_mut, write_pair, Array, Entry, HeapMemory, Unpadded};
use crate::catalog::VersionId;
use crate::segment::{SegmentError, SegmentStore};
use crate::source::IdDatum;

/// Writes the header and every array of `plan` into `memory`, which holds
/// exactly its layout's bytes.
pub(super) fn fill<E: IdDatum>(
    segments: &mut impl SegmentStore,
    version: VersionId,
    plan: &Plan,
    memory: &mut HeapMemory,
) -> Result<(), LoadError> {
    let region = memory.bytes_mut();
    let layout = &plan.layout;
    layout.write_header(region);
    if let Some(entry) = layout.entry(Array::RelationDictionary) {
        region[entry.range()].copy_from_slice(&plan.relations);
    }
    for placed in &plan.placed {
        let Some(entry) = layout.entry(placed.array) else { continue };
        let into = &mut region[entry.range()];
        let count = match placed.array {
            Array::IdPairs if placed.encoded_width != size_of::<IdPair<E>>() || !NATIVE_LE => {
                let mut pairs = PairSink::<E>::new(into, placed.encoded_width);
                copy_payload(segments, version, placed, &mut |data| pairs.push(data))?
            }
            _ => {
                let mut at = 0;
                copy_payload(segments, version, placed, &mut |data| {
                    into[at..at + data.len()].copy_from_slice(data);
                    at += data.len();
                    Ok(())
                })?
            }
        };
        if placed.array == Array::Tombstones && count != plan.id_space {
            let expected = plan.id_space;
            return Err(GraphError::TombstonesLength { expected, found: count }.into());
        }
        to_native(placed, into)?;
    }
    by_dense::<E>(region, layout.entry(Array::IdPairs), layout.entry(Array::ByDense))
}

/// Whether native layout is little-endian, the segments' byte order.
const NATIVE_LE: bool = cfg!(target_endian = "little");

/// Puts a copied array of scalars in native byte order: nothing on a
/// little-endian target.
fn to_native(placed: &Placed, bytes: &mut [u8]) -> Result<(), LoadError> {
    fn swap<T: Unpadded>(bytes: &mut [u8], from_le: fn(T) -> T) -> Result<(), LoadError> {
        typed_mut::<T>(bytes)?.iter_mut().for_each(|value| *value = from_le(*value));
        Ok(())
    }
    if NATIVE_LE {
        return Ok(());
    }
    match (placed.array, placed.encoded_width) {
        (Array::IdPairs, _) => Ok(()),
        (_, 4) => swap::<u32>(bytes, u32::from_le),
        _ => swap::<u64>(bytes, u64::from_le),
    }
}

/// Streams the payload of `placed`'s segment to `into`, after checking
/// its chunks are numbered `0..n` and its header (kind, element width, and
/// a payload of the planned length); returns the header's count. What
/// `into` returns stops the read.
fn copy_payload(
    segments: &mut impl SegmentStore,
    version: VersionId,
    placed: &Placed,
    into: &mut dyn FnMut(&[u8]) -> Result<(), LoadError>,
) -> Result<u64, LoadError> {
    let segment = placed.segment.as_str();
    let mut header = [0u8; SEGMENT_HEADER_LEN];
    let (mut have, mut count, mut received, mut expected) = (0, None, 0u64, 0);
    let mut failed = None;
    let read = segments.read_chunks(version, segment, &mut |chunk_no, mut data| {
        if chunk_no != expected {
            let chunk_no = expected;
            return Err(SegmentError::ChunkMissing {
                version,
                segment: segment.to_owned(),
                chunk_no,
            });
        }
        expected += 1;
        if have < SEGMENT_HEADER_LEN {
            let take = data.len().min(SEGMENT_HEADER_LEN - have);
            header[have..have + take].copy_from_slice(&data[..take]);
            (have, data) = (have + take, &data[take..]);
            if have == SEGMENT_HEADER_LEN {
                let width = placed.encoded_width;
                count = Some(placed.kind.check_header(&header, width, placed.payload)?);
            }
        }
        received += data.len() as u64;
        if received > placed.payload {
            let (expected, found) = (placed.payload, received);
            return Err(GraphError::SegmentLength { expected, found }.into());
        }
        if data.is_empty() {
            return Ok(());
        }
        into(data).map_err(|error| {
            failed = Some(error);
            SegmentError::Graph(GraphError::SegmentTruncated)
        })
    });
    if let Some(error) = failed {
        return Err(error);
    }
    read?;
    let count = count.ok_or(GraphError::SegmentTruncated)?;
    if received != placed.payload {
        let (expected, found) = (placed.payload, received);
        return Err(GraphError::SegmentLength { expected, found }.into());
    }
    Ok(count)
}

/// Re-lays out encoded id-map pairs into `IdPair<E>`s as they stream in:
/// an `int4` pair is 12 bytes encoded and 16 in memory, and on a
/// big-endian target every pair is reordered.
struct PairSink<'a, E> {
    slots: &'a mut [u8],
    encoded_width: usize,
    next: usize,
    carry: Vec<u8>,
    pair: std::marker::PhantomData<E>,
}

impl<'a, E: IdDatum> PairSink<'a, E> {
    fn new(slots: &'a mut [u8], encoded_width: usize) -> Self {
        let carry = Vec::with_capacity(encoded_width);
        Self { slots, encoded_width, next: 0, carry, pair: std::marker::PhantomData }
    }

    /// Takes the next bytes of the payload.
    fn push(&mut self, mut data: &[u8]) -> Result<(), LoadError> {
        let width = self.encoded_width;
        if !self.carry.is_empty() {
            let take = data.len().min(width - self.carry.len());
            self.carry.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.carry.len() < width {
                return Ok(());
            }
            let carried = std::mem::take(&mut self.carry);
            self.write(&carried)?;
            self.carry = carried;
            self.carry.clear();
        }
        let mut pairs = data.chunks_exact(width);
        for encoded in &mut pairs {
            self.write(encoded)?;
        }
        self.carry.extend_from_slice(pairs.remainder());
        Ok(())
    }

    /// Writes one encoded pair into the next slot.
    fn write(&mut self, encoded: &[u8]) -> Result<(), LoadError> {
        let size = size_of::<IdPair<E>>();
        let slot = self.slots.get_mut(self.next * size..(self.next + 1) * size);
        let slot = slot.ok_or(GraphError::SegmentTruncated)?;
        let (external, dense) = encoded.split_at(E::WIDTH);
        let mut word = [0; size_of::<DenseId>()];
        word.copy_from_slice(dense);
        let pair = IdPair { external: E::read_le(external), dense: DenseId::from_le_bytes(word) };
        write_pair(slot, pair)?;
        self.next += 1;
        Ok(())
    }
}

/// Builds the id map's second direction, each dense id's external id, from
/// its pairs: an error if a dense id is outside the id space (a repeated
/// one leaves another unset, which `IdMap::from_segments` refuses).
fn by_dense<E: IdDatum>(
    region: &mut [u8],
    pairs: Option<&Entry>,
    by_dense: Option<&Entry>,
) -> Result<(), LoadError> {
    let (Some(pairs), Some(by_dense)) = (pairs, by_dense) else {
        return Ok(());
    };
    let (before, after) = region.split_at_mut(by_dense.offset);
    let pairs = typed::<IdPair<E>>(&before[pairs.range()])?;
    let externals = typed_mut::<E>(&mut after[..by_dense.range().len()])?;
    for pair in pairs {
        let slot = usize::try_from(pair.dense).ok().and_then(|dense| externals.get_mut(dense));
        *slot.ok_or(GraphError::IdMapNotPermutation)? = pair.external;
    }
    Ok(())
}
