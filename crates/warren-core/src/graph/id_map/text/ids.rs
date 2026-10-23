//! The node ids a text rebuild takes: an arena of their bytes, with where
//! each ends, and an index array that sorting orders (DESIGN.md §5.7).

use crate::graph::build_memory::growth;
use crate::graph::{BuildBuffer, BuildMemory, GraphError, NodeIds};

/// Text node ids, duplicates allowed, in an arena charged to a build's
/// memory: 16 bytes an id beside its bytes.
#[derive(Debug)]
pub struct TextIds {
    /// Every id's bytes, in the order taken.
    bytes: BuildBuffer<u8>,
    /// Where each id's bytes end; each starts where the previous ends.
    ends: BuildBuffer<u64>,
    /// The ids by their position in `ends`: in the order taken, then sorted
    /// and deduplicated by [`NodeIds::sort_distinct`].
    order: BuildBuffer<u64>,
}

impl TextIds {
    /// No ids, charged to `memory`.
    pub fn new(memory: &BuildMemory) -> Self {
        Self {
            bytes: BuildBuffer::new(memory),
            ends: BuildBuffer::new(memory),
            order: BuildBuffer::new(memory),
        }
    }

    /// Appends an id. The arena grows geometrically while the budget
    /// allows; near its limit its bytes take at most half of what remains,
    /// and the ends and the order grow together by what still fits.
    pub fn push(&mut self, id: &[u8]) -> Result<(), GraphError> {
        if self.bytes.capacity_left() < id.len() {
            let room = growth(self.bytes.len(), self.bytes.memory(), 2).max(id.len());
            self.bytes.reserve_exact(room)?;
        }
        if self.ends.capacity_left() == 0 || self.order.capacity_left() == 0 {
            let room = growth(self.ends.len(), self.ends.memory(), 16);
            self.ends.reserve_exact(room)?;
            self.order.reserve_exact(room)?;
        }
        let position = self.ends.len() as u64;
        self.ends.push((self.bytes.len() + id.len()) as u64)?;
        self.order.push(position)?;
        self.bytes.extend(id.iter().copied());
        Ok(())
    }
}

/// The id at `position` of an arena's `ends`.
fn id<'a>(bytes: &'a [u8], ends: &[u64], position: u64) -> &'a [u8] {
    let position = position as usize;
    let start = match position {
        0 => 0,
        _ => ends.get(position - 1).copied().unwrap_or_default(),
    };
    let end = ends.get(position).copied().unwrap_or_default();
    bytes.get(start as usize..end as usize).unwrap_or_default()
}

impl NodeIds for TextIds {
    type Key = [u8];

    fn memory(&self) -> &BuildMemory {
        self.bytes.memory()
    }

    fn sort_distinct(&mut self) {
        let (bytes, ends) = (&self.bytes, &self.ends);
        self.order.sort_unstable_by(|&a, &b| id(bytes, ends, a).cmp(id(bytes, ends, b)));
        self.order.dedup_by(|a, b| id(bytes, ends, *a) == id(bytes, ends, *b));
    }

    fn keys(&self) -> impl Iterator<Item = &[u8]> {
        self.order.iter().map(|&position| id(&self.bytes, &self.ends, position))
    }
}
