//! Where a loaded segment's elements live: a buffer the graph version owns,
//! or a read-only view of memory that something else keeps mapped, such as
//! a shared version's DSM segment (DESIGN.md §5.6, §14 item 29).

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use super::{memory, GraphError};

/// Elements in memory that the implementor keeps readable, and unchanged,
/// for as long as it lives: a view into a shared version's mapping, which
/// holds the mapping, or a heap region in tests. `elements` returns the
/// same elements on every call.
pub trait SharedSegment<T>: Send + Sync {
    /// The elements.
    fn elements(&self) -> &[T];
}

/// A segment's elements: owned (a build's buffers, a decoded segment,
/// compaction, tests), or a shared, read-only view that keeps what it views
/// alive. Both read as a slice, so kernels do not tell them apart.
pub struct SegmentBuffer<T> {
    /// The elements, unless `shared` views them.
    owned: Vec<T>,
    shared: Option<Arc<dyn SharedSegment<T>>>,
}

impl<T> SegmentBuffer<T> {
    /// A read-only view of `segment`'s elements, which it keeps alive.
    pub fn shared(segment: Arc<dyn SharedSegment<T>>) -> Self {
        Self { owned: Vec::new(), shared: Some(segment) }
    }

    /// The elements.
    pub fn as_slice(&self) -> &[T] {
        match &self.shared {
            None => &self.owned,
            Some(segment) => segment.elements(),
        }
    }

    /// Whether it is a shared view rather than an owned buffer.
    pub(crate) fn is_shared(&self) -> bool {
        self.shared.is_some()
    }

    /// The bytes it holds: an owned buffer's capacity, or the bytes a
    /// shared view views (the region's owner accounts for the region).
    pub fn bytes(&self) -> u64 {
        match &self.shared {
            None => memory::held(&self.owned),
            Some(segment) => {
                let elements = segment.elements();
                (elements.len() as u64).saturating_mul(size_of::<T>() as u64)
            }
        }
    }
}

impl<T: Clone> SegmentBuffer<T> {
    /// The elements as an owned buffer that can be changed: a shared view
    /// is first copied (failing rather than aborting if memory is short),
    /// and the memory it views is left unchanged.
    pub(crate) fn make_mut(&mut self) -> Result<&mut Vec<T>, GraphError> {
        if let Some(segment) = &self.shared {
            let elements = segment.elements();
            let mut copy = memory::with_capacity(elements.len())?;
            copy.extend_from_slice(elements);
            self.owned = copy;
            self.shared = None;
        }
        Ok(&mut self.owned)
    }
}

impl<T> Default for SegmentBuffer<T> {
    fn default() -> Self {
        Self::from(Vec::new())
    }
}

impl<T> From<Vec<T>> for SegmentBuffer<T> {
    fn from(elements: Vec<T>) -> Self {
        Self { owned: elements, shared: None }
    }
}

impl<T> Deref for SegmentBuffer<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: Clone> Clone for SegmentBuffer<T> {
    fn clone(&self) -> Self {
        Self { owned: self.owned.clone(), shared: self.shared.clone() }
    }
}

impl<T: PartialEq> PartialEq for SegmentBuffer<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq> Eq for SegmentBuffer<T> {}

impl<T: fmt::Debug> fmt::Debug for SegmentBuffer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_slice(), f)
    }
}

/// A shared segment on the heap, standing in for a mapping in tests.
#[cfg(test)]
pub(crate) struct HeapSegment<T> {
    elements: Vec<T>,
}

#[cfg(test)]
impl<T: Clone> HeapSegment<T> {
    /// A heap segment holding a copy of `elements`.
    pub(crate) fn of(elements: &[T]) -> Arc<Self> {
        Arc::new(Self { elements: elements.to_vec() })
    }
}

#[cfg(test)]
impl<T: Send + Sync> SharedSegment<T> for HeapSegment<T> {
    fn elements(&self) -> &[T] {
        &self.elements
    }
}

#[cfg(test)]
mod tests;
