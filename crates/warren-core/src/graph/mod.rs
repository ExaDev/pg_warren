//! Graph structure: the immutable CSR snapshot of one graph version.
//!
//! Owns forward/reverse CSR per relation, the external-id ↔ dense-id map with
//! stable dense ids across versions, and the tombstone bitmap
//! (DESIGN.md §5.2, §5.3).

mod error;
mod memory;
mod tombstones;

pub use error::GraphError;
pub use tombstones::Tombstones;

/// A dense id: a node's position in the id space of a graph (DESIGN.md §5.2).
pub type DenseId = u64;
