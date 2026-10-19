//! The spectral feature's error type.

use thiserror::Error;

use crate::compute::ComputeError;
use crate::graph::DenseId;

/// Why an eigenproblem or a distance could not be computed.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum SpectralError {
    /// A compute kernel failed, ran out of budget or was cancelled.
    #[error(transparent)]
    Compute(#[from] ComputeError),
    /// An operator name is not one of the §6.5 matrices.
    #[error(
        "unknown matrix \"{0}\": expected adjacency, laplacian, normalized_laplacian or random_walk"
    )]
    UnknownOperator(String),
    /// A `which` name is not `largest` or `smallest`.
    #[error("unknown which \"{0}\": expected largest or smallest")]
    UnknownWhich(String),
    /// `symmetrize = false` needs a symmetric matrix (Arnoldi, for directed
    /// graphs, is v1.1).
    #[error("the matrix is not symmetric: use symmetrize, or an undirected graph")]
    NotSymmetric,
    /// The normalised Laplacian and the random walk divide by degrees, so
    /// every node with an edge needs a positive (weighted) degree.
    #[error("node {node} has non-positive degree {degree}")]
    NonPositiveDegree {
        /// The dense id of the node.
        node: DenseId,
        /// Its degree.
        degree: f64,
    },
    /// There must be between 1 and as many eigenpairs as active nodes.
    #[error("cannot compute {k} eigenpairs of a matrix of dimension {dimension}")]
    EigenpairCount {
        /// The eigenpairs requested.
        k: usize,
        /// The number of active nodes.
        dimension: u64,
    },
    /// A value overflowed (weights too large) and the iteration cannot
    /// continue.
    #[error("the eigenproblem produced a non-finite value")]
    NotFinite,
    /// A subspace needs at least one vector.
    #[error("a subspace needs at least one vector")]
    EmptySubspace,
    /// The vectors spanning a subspace must be linearly independent.
    #[error("the vectors spanning a subspace are linearly dependent")]
    RankDeficient,
    /// An alignment mapping needs one entry per row of the vector.
    #[error("the mapping has {found} entries, expected one per row: {expected}")]
    MappingLength {
        /// The vector's id space.
        expected: u64,
        /// The mapping's length.
        found: u64,
    },
    /// An alignment mapping names a row past the target id space.
    #[error("dense id {target} is outside the id space {id_space}")]
    MappedOutside {
        /// The target dense id.
        target: DenseId,
        /// The target id space.
        id_space: u64,
    },
    /// An alignment mapping must be one-to-one.
    #[error("two rows map to dense id {target}")]
    MappedTwice {
        /// The target dense id.
        target: DenseId,
    },
}
