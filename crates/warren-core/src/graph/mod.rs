//! Graph structure: the immutable CSR snapshot of one graph version.
//!
//! Owns forward/reverse CSR per relation, the external-id ↔ dense-id map with
//! stable dense ids across versions, and the tombstone bitmap
//! (DESIGN.md §5.2, §5.3), and their encoding as segments.
//!
//! A build runs: [`IdMap::rebuild`] over the node ids → an [`EdgeIntake`]
//! over the edge rows (dropping and counting dangling edges) →
//! [`GraphCsr::build`] → `encode` on each part. Loading runs the matching
//! `decode`s, which validate each segment, then [`RelationCsr::new`] and
//! [`GraphCsr::from_parts`], which validate the segments against each other.

pub(crate) mod build_memory;
mod csr;
mod direction;
pub(crate) mod edge_list;
mod error;
mod id_map;
pub(crate) mod memory;
mod node_order;
mod relation;
mod segment;
mod tombstones;

pub use build_memory::{BuildBuffer, BuildCharge, BuildMemory};
pub use csr::{
    AllRelations, AnyCsr, BuildPlan, BuildSummary, BuiltCsr, Csr, CsrDirection, CsrLayout, CsrRows,
    CsrScope, CsrSink, GraphCsr, Neighbours, RelationCsr, RowNeighbours, Target, TargetWidth,
};
pub use direction::Direction;
pub use edge_list::{EdgeIntake, EdgeList};
pub use error::GraphError;
pub use id_map::{ExternalId, IdMap};
pub use node_order::{NodeOrder, Permutation};
pub use relation::{RelationDictionary, RelationIndex, RelationSelection};
pub use segment::{ChunkSink, CsrPart, CsrSegments};
pub use tombstones::Tombstones;

/// A dense id: a node's position in the id space of a graph (DESIGN.md §5.2).
pub type DenseId = u64;
