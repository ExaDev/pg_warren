//! Encoding of the tombstone bitmap as its `tombstones` segment.

use super::{elements, read_header, with_header, Element, Header, SegmentKind};
use crate::graph::{GraphError, Tombstones};

impl Tombstones {
    /// Encodes the bitmap; the header's count is the id space in bits.
    pub fn encode(&self) -> Result<Vec<u8>, GraphError> {
        let words = self.words();
        let mut bytes = with_header(
            SegmentKind::Tombstones,
            u64::WIDTH,
            self.id_space(),
            words.len() * u64::WIDTH,
        )?;
        words.iter().for_each(|word| word.write(&mut bytes));
        Ok(bytes)
    }

    /// Decodes a bitmap, rejecting set bits beyond the id space.
    pub fn decode(bytes: &[u8]) -> Result<Self, GraphError> {
        let (header, payload) = read_header(bytes, SegmentKind::Tombstones)?;
        let id_space = header.count;
        let words = Header { width: header.width, count: id_space.div_ceil(64) };
        Self::from_words(id_space, elements(&words, payload)?)
    }
}
