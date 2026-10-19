//! The version feature's error type.

use thiserror::Error;

use crate::graph::{DenseId, GraphError};

/// Why an operation across graph versions could not run.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VersionError {
    /// Building or allocating graph structure failed.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// A version's CSRs must cover exactly its id map's id space.
    #[error("id map covers {id_map} dense ids but the CSRs cover {csr}")]
    IdSpaceMismatch {
        /// The id map's id space.
        id_map: u64,
        /// The CSRs' id space.
        csr: u64,
    },
    /// A version's CSRs must hold one relation per dictionary entry.
    #[error("relation dictionary has {dictionary} relations but the CSRs have {csr}")]
    RelationCountMismatch {
        /// The number of relations in the dictionary.
        dictionary: usize,
        /// The number of per-relation CSRs.
        csr: usize,
    },
    /// A later version's id space never shrinks.
    #[error("the later version's id space ({after}) is smaller than the earlier's ({before})")]
    IdSpaceShrank {
        /// The earlier version's id space.
        before: u64,
        /// The later version's id space.
        after: u64,
    },
    /// Two versions compared by dense id must give each dense id of the
    /// earlier version the same external id: not true across a compaction
    /// boundary or between graphs.
    #[error("dense id {dense} names different nodes in the two versions")]
    IdSpacesDiverge {
        /// The first dense id whose external ids differ.
        dense: DenseId,
    },
    /// A live edge must not touch a tombstoned node.
    #[error("edge {from} → {to} touches a tombstoned node")]
    EdgeTouchesTombstone {
        /// The edge's source (named `from`: thiserror reserves `source`).
        from: DenseId,
        /// The edge's target.
        to: DenseId,
    },
}
