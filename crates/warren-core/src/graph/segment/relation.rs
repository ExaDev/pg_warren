//! Encoding of the relation dictionary as its `rel_dict` segment.

use super::{read_header, with_header, Element, SegmentKind};
use crate::graph::{GraphError, RelationDictionary};

impl RelationDictionary {
    /// Encodes the names in index order, each as a `u32` byte length
    /// followed by UTF-8.
    pub fn encode(&self) -> Result<Vec<u8>, GraphError> {
        let payload_len = self.names().iter().map(|name| 4 + name.len()).sum();
        let mut bytes =
            with_header(SegmentKind::RelationDictionary, 0, self.len() as u64, payload_len)?;
        for name in self.names() {
            let len = u32::try_from(name.len()).map_err(|_| GraphError::RelationNameTooLong)?;
            len.write(&mut bytes);
            bytes.extend_from_slice(name.as_bytes());
        }
        Ok(bytes)
    }

    /// Decodes a dictionary, rejecting invalid UTF-8 and duplicate names.
    pub fn decode(bytes: &[u8]) -> Result<Self, GraphError> {
        let (header, mut payload) = read_header(bytes, SegmentKind::RelationDictionary)?;
        let mut relations = Self::new();
        for _ in 0..header.count {
            let (len, rest) = payload.split_at_checked(4).ok_or(GraphError::SegmentTruncated)?;
            let (name, rest) = rest
                .split_at_checked(u32::read(len) as usize)
                .ok_or(GraphError::SegmentTruncated)?;
            let name = std::str::from_utf8(name).map_err(|_| GraphError::RelationNameNotUtf8)?;
            if relations.get(name).is_some() {
                return Err(GraphError::DuplicateRelationName);
            }
            relations.intern(name)?;
            payload = rest;
        }
        if !payload.is_empty() {
            return Err(GraphError::SegmentLength {
                expected: (bytes.len() - payload.len()) as u64,
                found: bytes.len() as u64,
            });
        }
        Ok(relations)
    }
}
