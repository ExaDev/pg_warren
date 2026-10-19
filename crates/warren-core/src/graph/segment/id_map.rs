//! Encoding of an id map as its `id_map` segment.

use super::{decode, encode, SegmentKind};
use crate::graph::{ExternalId, GraphError, IdMap, Tombstones};

impl<E: ExternalId> IdMap<E> {
    /// Encodes the (external id, dense id) pairs as the `id_map` segment.
    /// The tombstones are a segment of their own.
    pub fn encode(&self) -> Result<Vec<u8>, GraphError> {
        encode(SegmentKind::IdMap, self.pairs())
    }

    /// Decodes an id map from its `id_map` segment and its tombstones,
    /// checking that the pairs are strictly sorted by external id, that the
    /// dense ids are exactly `0..id_space`, and that the tombstones cover
    /// the same id space.
    pub fn decode(bytes: &[u8], tombstones: Tombstones) -> Result<Self, GraphError> {
        Self::from_parts(decode(SegmentKind::IdMap, bytes)?, tombstones)
    }
}
