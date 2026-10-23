//! Encoding of the tombstone bitmap as its `tombstones` segment.

use super::{
    elements, encode_chunked, read_header, with_header, ChunkSink, Element, Header, SegmentKind,
};
use crate::graph::{BuildMemory, GraphError, Tombstones};

impl Tombstones {
    /// Encodes the bitmap; the header's count is the id space in bits.
    pub fn encode(&self) -> Result<Vec<u8>, GraphError> {
        let words = self.words();
        let mut bytes = with_header(
            SegmentKind::Tombstones,
            u64::WIDTH,
            self.id_space(),
            words.len() * u64::WIDTH,
            0,
        )?;
        words.iter().for_each(|word| word.write(&mut bytes));
        Ok(bytes)
    }

    /// [`Self::encode`] in chunks of `chunk_size` bytes, through a chunk
    /// buffer charged to `memory`; returns the segment's length.
    pub fn encode_chunked<X: From<GraphError>>(
        &self,
        chunk_size: usize,
        memory: &BuildMemory,
        each: &mut ChunkSink<'_, X>,
    ) -> Result<u64, X> {
        let kind = SegmentKind::Tombstones;
        encode_chunked(kind, self.id_space(), self.words(), chunk_size, memory, each)
    }

    /// Decodes a bitmap, rejecting set bits beyond the id space.
    pub fn decode(bytes: &[u8]) -> Result<Self, GraphError> {
        let (header, payload) = read_header(bytes, SegmentKind::Tombstones)?;
        let id_space = header.count;
        let words = Header { count: id_space.div_ceil(64), ..header };
        Self::from_words(id_space, elements(&words, payload)?)
    }
}
