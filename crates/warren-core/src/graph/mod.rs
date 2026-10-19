//! Graph structure: the immutable CSR snapshot of one graph version.
//!
//! Owns forward/reverse CSR per relation, the external-id ↔ dense-id map with
//! stable dense ids across versions, and the tombstone bitmap
//! (DESIGN.md §5.2, §5.3).

mod csr;
mod edge_list;
mod error;
mod id_map;
mod memory;
mod relation;
mod segment;
mod tombstones;

pub use csr::{AnyCsr, Csr, GraphCsr, Neighbours, RelationCsr, Target};
pub use edge_list::{EdgeIntake, EdgeList};
pub use error::GraphError;
pub use id_map::{ExternalId, IdMap};
pub use relation::{RelationDictionary, RelationIndex};
pub use segment::{CsrSegments, SegmentKind};
pub use tombstones::Tombstones;

/// A dense id: a node's position in the id space of a graph (DESIGN.md §5.2).
pub type DenseId = u64;
