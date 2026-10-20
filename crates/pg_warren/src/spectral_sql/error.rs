//! The spectral bindings' error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;
use warren_core::spectral::SpectralError;
use warren_core::version::VersionError;

use crate::access::SqlError;
use crate::cache::CacheError;
use crate::catalog::{CatalogError, VersionId};
use crate::compute_sql::{compute_sqlstate, ComputeSqlError};

/// Why `eigen` or `distance` could not answer.
#[derive(Debug, Error)]
pub(crate) enum SpectralSqlError {
    /// An argument that must be given was NULL.
    #[error("{0} must not be NULL")]
    NullArgument(&'static str),
    /// An argument is out of its range.
    #[error("{0}")]
    InvalidArgument(String),
    /// The two versions are in different id spaces (DESIGN.md §5.2).
    #[error(
        "versions {from} and {to} of graph \"{graph}\" are in different id spaces: \
         version {compacted} was compacted from version {source_version}"
    )]
    CompactionBoundary {
        /// The graph.
        graph: String,
        /// The first version.
        from: VersionId,
        /// The second version.
        to: VersionId,
        /// The compacted version between them.
        compacted: VersionId,
        /// The version it was compacted from.
        source_version: VersionId,
    },
    /// Graphs compared by external id must share an id type.
    #[error("graphs \"{0}\" and \"{1}\" have different id types")]
    IdTypeMismatch(String, String),
    /// The kernel refused or failed.
    #[error("{0}")]
    Spectral(#[from] SpectralError),
    /// The versions could not be aligned.
    #[error(transparent)]
    Version(#[from] VersionError),
    /// A vector, the graph or a version could not be read or written.
    #[error(transparent)]
    Compute(#[from] ComputeSqlError),
    /// The graph or version does not exist.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// The graph or version could not be opened.
    #[error(transparent)]
    Cache(#[from] CacheError),
}

impl From<warren_core::compute::ComputeError> for SpectralSqlError {
    fn from(error: warren_core::compute::ComputeError) -> Self {
        Self::Spectral(error.into())
    }
}

impl SqlError for SpectralSqlError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::NullArgument(_) => PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED,
            Self::InvalidArgument(_) | Self::CompactionBoundary { .. } | Self::Version(_) => {
                PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE
            }
            Self::IdTypeMismatch(..) => PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH,
            Self::Spectral(SpectralError::Compute(error)) => compute_sqlstate(error),
            Self::Spectral(SpectralError::WorkingSet { .. }) => {
                PgSqlErrorCode::ERRCODE_CONFIGURATION_LIMIT_EXCEEDED
            }
            Self::Spectral(SpectralError::NotFinite) => {
                PgSqlErrorCode::ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE
            }
            Self::Spectral(_) => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            Self::Compute(error) => error.sqlstate(),
            Self::Catalog(error) => error.sqlstate(),
            Self::Cache(error) => error.sqlstate(),
        }
    }
}
