//! The community bindings' error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;
use warren_core::community::CommunityError;
use warren_core::compute::ComputeError;
use warren_core::graph::GraphError;

use crate::access::{AccessError, SqlError};
use crate::build::BuildError;
use crate::cache::{CacheError, LoadError};
use crate::catalog::{CatalogError, VersionId};
use crate::compute_sql::{compute_sqlstate, ComputeSqlError};
use crate::extension::graph_sqlstate;
use crate::segment::SegmentError;

/// Why a contraction, projection or lift could not run.
#[derive(Debug, Error)]
pub(crate) enum CommunitySqlError {
    /// An argument that must be given was NULL.
    #[error("{0} must not be NULL")]
    NullArgument(&'static str),
    /// An argument is out of its range.
    #[error("{0}")]
    InvalidArgument(String),
    /// A vector of labels or keys does not hold integers.
    #[error("{what}: {error}")]
    Integers {
        /// The argument.
        what: &'static str,
        /// Why.
        error: CommunityError,
    },
    /// `warren.contract` adds versions only to a graph it derives.
    #[error(
        "graph \"{0}\" is built from queries, not derived by contraction: contract into a new \
         graph, or into one warren.contract created"
    )]
    NotAContraction(String),
    /// `project` and `lift` need a version derived by contraction.
    #[error("version {version} of graph \"{graph}\" was not derived by contraction")]
    NotDerived {
        /// The graph.
        graph: String,
        /// Its version.
        version: VersionId,
    },
    /// The parent version a quotient version was contracted from is gone.
    #[error(
        "version {version} of graph \"{graph}\" was contracted from a version that has since \
         been dropped"
    )]
    ParentDropped {
        /// The quotient graph.
        graph: String,
        /// Its version.
        version: VersionId,
    },
    /// A quotient version was contracted from another graph's version.
    #[error(
        "version {version} of graph \"{quotient}\" was contracted from version {parent_version} \
         of graph \"{parent}\", not of graph \"{graph}\""
    )]
    OtherParent {
        /// The quotient graph.
        quotient: String,
        /// Its version.
        version: VersionId,
        /// The version it was contracted from.
        parent_version: VersionId,
        /// That version's graph.
        parent: String,
        /// The graph named instead.
        graph: String,
    },
    /// A quotient version was contracted from another graph's version,
    /// which the caller may not read, so it is not named.
    #[error("version {version} of graph \"{quotient}\" was not contracted from graph \"{graph}\"")]
    NotContractedFrom {
        /// The quotient graph.
        quotient: String,
        /// Its version.
        version: VersionId,
        /// The graph named.
        graph: String,
    },
    /// A community kernel failed.
    #[error(transparent)]
    Community(#[from] CommunityError),
    /// The quotient version could not be built or written.
    #[error(transparent)]
    Build(#[from] BuildError),
    /// A vector could not be read or written.
    #[error(transparent)]
    ComputeSql(#[from] ComputeSqlError),
    /// A graph or version could not be found, created or locked.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// The caller may not do this.
    #[error(transparent)]
    Access(#[from] AccessError),
    /// A version could not be opened.
    #[error(transparent)]
    Cache(#[from] CacheError),
    /// A segment could not be read.
    #[error(transparent)]
    Segment(#[from] SegmentError),
    /// A structure could not be built or decoded.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// The compute pool could not start, or the work was cancelled.
    #[error(transparent)]
    Compute(#[from] ComputeError),
    /// An SPI call failed.
    #[error(transparent)]
    Spi(#[from] pgrx::spi::Error),
}

impl From<LoadError> for CommunitySqlError {
    fn from(error: LoadError) -> Self {
        Self::Cache(error.into())
    }
}

impl SqlError for CommunitySqlError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::NullArgument(_) => PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED,
            Self::InvalidArgument(_)
            | Self::OtherParent { .. }
            | Self::NotContractedFrom { .. } => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            Self::Integers { error, .. } | Self::Community(error) => community_sqlstate(error),
            Self::NotAContraction(_) | Self::NotDerived { .. } => {
                PgSqlErrorCode::ERRCODE_WRONG_OBJECT_TYPE
            }
            Self::ParentDropped { .. } => PgSqlErrorCode::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE,
            Self::Build(error) => error.sqlstate(),
            Self::ComputeSql(error) => error.sqlstate(),
            Self::Catalog(error) => error.sqlstate(),
            Self::Access(error) => error.sqlstate(),
            Self::Cache(error) => error.sqlstate(),
            Self::Segment(error) => error.sqlstate(),
            Self::Graph(error) => graph_sqlstate(error),
            Self::Compute(error) => compute_sqlstate(error),
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        }
    }
}

/// The SQLSTATE of a community kernel's error.
fn community_sqlstate(error: &CommunityError) -> PgSqlErrorCode {
    match error {
        CommunityError::Graph(error) => graph_sqlstate(error),
        CommunityError::Compute(error) => compute_sqlstate(error),
        CommunityError::LabelDtype(_) => PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH,
        CommunityError::LabelWidth(_)
        | CommunityError::NotIntegral { .. }
        | CommunityError::UnknownAggregate(_)
        | CommunityError::NegativeWeight { .. } => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
        CommunityError::LabelLength { .. } | CommunityError::UnknownLabel { .. } => {
            PgSqlErrorCode::ERRCODE_INTERNAL_ERROR
        }
    }
}
