//! A batch's endpoint column: each row's external id, or NULL, copied into
//! buffers charged to the build's memory.

use std::mem::size_of;

use crate::graph::build_memory::growth;
use crate::graph::{BuildBuffer, BuildMemory, ExternalId, GraphError};

/// One endpoint column of an [`EdgeBatch`](super::EdgeBatch).
pub trait Endpoints: Sync + Send {
    /// An external id.
    type Key: ?Sized;

    /// The bytes a row takes, beyond a text id's own.
    const ROW_BYTES: u64;

    /// An empty column with room for `rows` rows (but not their text ids'
    /// bytes), charged to `memory`, which keeps room for up to `id_bytes`
    /// of text ids when it is cleared.
    fn with_capacity(memory: &BuildMemory, rows: usize, id_bytes: u64) -> Result<Self, GraphError>
    where
        Self: Sized;

    /// Appends a row's endpoint.
    fn push(&mut self, key: Option<&Self::Key>) -> Result<(), GraphError>;

    /// Row `row`'s endpoint, if it is not NULL.
    fn get(&self, row: usize) -> Option<&Self::Key>;

    /// Empties it, keeping its capacity, except text ids' room past the
    /// bytes it was made to keep.
    fn clear(&mut self);

    /// The bytes it holds, capacity included.
    fn bytes(&self) -> u64;

    /// The bytes of the text ids it holds, capacity excluded: 0 for a
    /// fixed type.
    fn len_bytes(&self) -> u64;
}

/// The endpoints of a fixed-width id type.
#[derive(Debug)]
pub struct FixedEndpoints<E>(BuildBuffer<Option<E>>);

impl<E: ExternalId + Send + Sync> Endpoints for FixedEndpoints<E> {
    type Key = E;
    const ROW_BYTES: u64 = size_of::<Option<E>>() as u64;

    fn with_capacity(memory: &BuildMemory, rows: usize, _: u64) -> Result<Self, GraphError> {
        Ok(Self(BuildBuffer::with_capacity(memory, rows)?))
    }

    fn push(&mut self, key: Option<&E>) -> Result<(), GraphError> {
        self.0.push(key.copied())
    }

    fn get(&self, row: usize) -> Option<&E> {
        self.0.get(row)?.as_ref()
    }

    fn clear(&mut self) {
        self.0.truncate(0);
    }

    fn bytes(&self) -> u64 {
        self.0.bytes()
    }

    fn len_bytes(&self) -> u64 {
        0
    }
}

/// The endpoints of text ids: their bytes, where each row's ends, and
/// which rows are NULL.
#[derive(Debug)]
pub struct TextEndpoints {
    bytes: BuildBuffer<u8>,
    /// Where each row's bytes end; each starts where the previous ends.
    ends: BuildBuffer<usize>,
    nulls: BuildBuffer<bool>,
    /// The room for text ids' bytes kept when it is cleared.
    kept: u64,
}

impl Endpoints for TextEndpoints {
    type Key = [u8];
    const ROW_BYTES: u64 = (size_of::<usize>() + size_of::<bool>()) as u64;

    fn with_capacity(memory: &BuildMemory, rows: usize, id_bytes: u64) -> Result<Self, GraphError> {
        Ok(Self {
            bytes: BuildBuffer::new(memory),
            ends: BuildBuffer::with_capacity(memory, rows)?,
            nulls: BuildBuffer::with_capacity(memory, rows)?,
            kept: id_bytes,
        })
    }

    fn push(&mut self, key: Option<&[u8]>) -> Result<(), GraphError> {
        let id = key.unwrap_or_default();
        if self.bytes.capacity_left() < id.len() {
            let room = growth(self.bytes.len(), self.bytes.memory(), 1).max(id.len());
            self.bytes.reserve_exact(room)?;
        }
        self.bytes.extend(id.iter().copied());
        self.ends.push(self.bytes.len())?;
        self.nulls.push(key.is_none())
    }

    fn get(&self, row: usize) -> Option<&[u8]> {
        if *self.nulls.get(row)? {
            return None;
        }
        let start = match row {
            0 => 0,
            _ => self.ends[row - 1],
        };
        self.bytes.get(start..self.ends[row])
    }

    fn clear(&mut self) {
        match self.bytes.bytes() > self.kept {
            true => self.bytes.release(),
            false => self.bytes.truncate(0),
        }
        self.ends.truncate(0);
        self.nulls.truncate(0);
    }

    fn bytes(&self) -> u64 {
        self.bytes.bytes() + self.ends.bytes() + self.nulls.bytes()
    }

    fn len_bytes(&self) -> u64 {
        self.bytes.len() as u64
    }
}
