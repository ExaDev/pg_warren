//! Encoding of an id map as its `id_map` segment.

use std::mem::size_of;

use super::{decode, encode, encode_chunked, ChunkSink, Element, SegmentKind, SEGMENT_HEADER_LEN};
use crate::graph::{BuildCharge, BuildMemory, ExternalId, GraphError, IdMap, IdPair, Tombstones};

impl<E: ExternalId> IdMap<E> {
    /// Encodes the (external id, dense id) pairs as the `id_map` segment.
    /// The tombstones are a segment of their own.
    pub fn encode(&self) -> Result<Vec<u8>, GraphError> {
        encode(SegmentKind::IdMap, self.pairs())
    }

    /// [`Self::encode`] in chunks of `chunk_size` bytes, through a chunk
    /// buffer charged to `memory`; returns the segment's length.
    pub fn encode_chunked<X: From<GraphError>>(
        &self,
        chunk_size: usize,
        memory: &BuildMemory,
        each: &mut ChunkSink<'_, X>,
    ) -> Result<u64, X> {
        let pairs = self.pairs();
        encode_chunked(SegmentKind::IdMap, pairs.len() as u64, pairs, chunk_size, memory, each)
    }

    /// [`Self::decode`] of the `id_map` and `tombstones` segments, charging
    /// `memory` before decoding for the decoded bitmap, the pairs, the
    /// inverse map and the scratch that builds it; returns the charge for
    /// the id map it holds.
    pub fn decode_within(
        id_map: &[u8],
        tombstones: &[u8],
        memory: &BuildMemory,
    ) -> Result<(Self, BuildCharge), GraphError> {
        let mut charge = memory.charge(tombstones.len() as u64)?;
        let tombstones = Tombstones::decode(tombstones)?;
        let pairs = (id_map.len().saturating_sub(SEGMENT_HEADER_LEN) / IdPair::<E>::WIDTH) as u64;
        let per_pair = size_of::<IdPair<E>>() + size_of::<Option<E>>() + size_of::<E>();
        charge.grow(pairs.saturating_mul(per_pair as u64))?;
        let id_map = Self::decode(id_map, tombstones)?;
        charge.shrink(charge.bytes().saturating_sub(id_map.bytes()));
        Ok((id_map, charge))
    }

    /// Decodes an id map from its `id_map` segment and its tombstones,
    /// checking that the pairs are strictly sorted by external id, that the
    /// dense ids are exactly `0..id_space`, and that the tombstones cover
    /// the same id space.
    pub fn decode(bytes: &[u8], tombstones: Tombstones) -> Result<Self, GraphError> {
        Self::from_parts(decode(SegmentKind::IdMap, bytes)?, tombstones)
    }
}
