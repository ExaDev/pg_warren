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
    /// A live edge must not touch a tombstoned node.
    #[error("edge {from} → {to} touches a tombstoned node")]
    EdgeTouchesTombstone {
        /// The edge's source (named `from`: thiserror reserves `source`).
        from: DenseId,
        /// The edge's target.
        to: DenseId,
    },
}
