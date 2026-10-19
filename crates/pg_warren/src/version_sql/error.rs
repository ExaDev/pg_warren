//! The error type of operations across versions.

use pgrx::PgSqlErrorCode;
use thiserror::Error;
use warren_core::version::VersionError;

use crate::access::AccessError;
use crate::build::BuildError;
use crate::cache::LoadError;
use crate::catalog::{CatalogError, VersionId};
use crate::extension::SqlError;
use crate::segment::SegmentError;

/// Why `compact` or `diff` could not run.
#[derive(Debug, Error)]
pub(crate) enum VersionOperationError {
    /// The graph has never been promoted.
    #[error("graph \"{0}\" has no current version to compact")]
    NoCurrentVersion(String),
    /// The two versions are in different id spaces (DESIGN.md §5.2).
    #[error(
        "versions {from} and {to} of graph \"{graph}\" are in different id spaces: \
         version {compacted} was compacted from version {source_version}"
    )]
    CompactionBoundary {
        /// The graph.
        graph: String,
        /// The earlier version asked for.
        from: VersionId,
        /// The later version asked for.
        to: VersionId,
        /// The compacted version between them.
        compacted: VersionId,
        /// The version it was compacted from.
        source_version: VersionId,
    },
    /// The graph or a version could not be found or recorded.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// The caller may not do this.
    #[error(transparent)]
    Access(#[from] AccessError),
    /// A version could not be loaded.
    #[error(transparent)]
    Load(#[from] LoadError),
    /// The compacted version could not be written.
    #[error(transparent)]
    Segment(#[from] SegmentError),
    /// The compacted version could not be recorded.
    #[error(transparent)]
    Build(#[from] BuildError),
    /// `warren-core` refused the operation.
    #[error(transparent)]
    Version(#[from] VersionError),
}

impl SqlError for VersionOperationError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::NoCurrentVersion(_) => PgSqlErrorCode::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE,
            Self::CompactionBoundary { .. } | Self::Version(_) => {
                PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE
            }
            Self::Catalog(error) => error.sqlstate(),
            Self::Access(error) => error.sqlstate(),
            Self::Load(error) => error.sqlstate(),
            Self::Segment(error) => error.sqlstate(),
            Self::Build(error) => error.sqlstate(),
        }
    }
}
