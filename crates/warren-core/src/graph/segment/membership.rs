//! Encoding of a contraction's membership as its `membership` segment
//! (DESIGN.md §6.6): one `u64` per parent node, its community in the
//! quotient version, or `u64::MAX` for none.

use super::{decode, encode, encode_chunked, ChunkSink, SegmentKind};
use crate::graph::{BuildMemory, GraphError, Membership};

impl Membership {
    /// Encodes the communities as the `membership` segment; the header's
    /// count is the parent version's id space.
    pub fn encode(&self) -> Result<Vec<u8>, GraphError> {
        encode(SegmentKind::Membership, self.stored())
    }

    /// [`Self::encode`] in chunks of `chunk_size` bytes, through a chunk
    /// buffer charged to `memory`; returns the segment's length.
    pub fn encode_chunked<X: From<GraphError>>(
        &self,
        chunk_size: usize,
        memory: &BuildMemory,
        each: &mut ChunkSink<'_, X>,
    ) -> Result<u64, X> {
        let stored = self.stored();
        let kind = SegmentKind::Membership;
        encode_chunked(kind, stored.len() as u64, stored, chunk_size, memory, each)
    }

    /// Decodes a membership from its segment, checking that every
    /// community lies below `quotient_id_space`.
    pub fn decode(bytes: &[u8], quotient_id_space: u64) -> Result<Self, GraphError> {
        Self::from_stored(decode(SegmentKind::Membership, bytes)?, quotient_id_space)
    }
}

#[cfg(test)]
mod tests {
    use super::super::HEADER_LEN;
    use super::*;

    fn membership() -> Membership {
        Membership::new(&[Some(1), None, Some(0)], 2).unwrap()
    }

    #[test]
    fn a_membership_round_trips_through_its_segment() {
        let bytes = membership().encode().unwrap();
        assert_eq!(&bytes[..8], b"WRSG\x01\x07\x08\x00");
        assert_eq!(&bytes[8..16], &3u64.to_le_bytes());
        assert_eq!(bytes.len(), HEADER_LEN + 3 * 8);
        assert_eq!(Membership::decode(&bytes, 2), Ok(membership()));
    }

    #[test]
    fn decoding_checks_the_communities_against_the_quotient() {
        let bytes = membership().encode().unwrap();
        assert_eq!(
            Membership::decode(&bytes, 1),
            Err(GraphError::CommunityOutsideQuotient { node: 0, community: 1 })
        );
    }

    #[test]
    fn chunks_concatenate_to_the_whole_segment() {
        let mut chunks = Vec::new();
        let length = membership()
            .encode_chunked::<GraphError>(7, &BuildMemory::unlimited(), &mut |chunk| {
                chunks.push(chunk.to_vec());
                Ok(())
            })
            .unwrap();
        assert!(chunks.iter().all(|chunk| chunk.len() <= 7));
        assert_eq!(chunks.concat(), membership().encode().unwrap());
        assert_eq!(length, chunks.concat().len() as u64);
    }
}
