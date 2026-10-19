//! The catalog feature's error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;

use super::VersionId;
use crate::access::AccessError;
use crate::access::SqlError;
use crate::source::SourceError;

/// Why a graph definition could not be created, found or used.
#[derive(Debug, Error)]
pub(crate) enum CatalogError {
    /// No graph has this name.
    #[error("graph \"{0}\" does not exist")]
    GraphNotFound(String),
    /// The version is not a version of the graph.
    #[error("graph \"{graph}\" has no version {version}")]
    VersionNotFound {
        /// The graph.
        graph: String,
        /// The version asked for.
        version: VersionId,
    },
    /// The version is in an id space a later compaction replaced
    /// (DESIGN.md §5.2): it may not become current or be built on.
    #[error(
        "version {version} of graph \"{graph}\" is in an id space that compacted version \
         {compacted} replaced"
    )]
    SupersededIdSpace {
        /// The graph.
        graph: String,
        /// The version asked for.
        version: VersionId,
        /// The latest compacted version after it.
        compacted: VersionId,
    },
    /// The current version cannot be dropped.
    #[error("cannot drop version {version} of graph \"{graph}\": it is the current version; promote another first")]
    DropCurrent {
        /// The graph.
        graph: String,
        /// Its current version.
        version: VersionId,
    },
    /// A graph with this name exists already.
    #[error("graph \"{0}\" already exists")]
    GraphExists(String),
    /// `retain_versions` must keep at least the current version.
    #[error("retain_versions must be at least 1, not {0}")]
    RetainVersions(i32),
    /// A count does not fit its `bigint` column.
    #[error("{0} exceeds the range of bigint")]
    TooLarge(&'static str),
    /// The caller may not do this, or an identity switch failed.
    #[error(transparent)]
    Access(#[from] AccessError),
    /// A source query does not fit the graph.
    #[error(transparent)]
    Source(#[from] SourceError),
    /// An SPI call failed.
    #[error(transparent)]
    Spi(#[from] pgrx::spi::Error),
}

impl SqlError for CatalogError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::GraphNotFound(_) | Self::VersionNotFound { .. } => {
                PgSqlErrorCode::ERRCODE_UNDEFINED_OBJECT
            }
            Self::DropCurrent { .. } => PgSqlErrorCode::ERRCODE_OBJECT_IN_USE,
            Self::SupersededIdSpace { .. } => {
                PgSqlErrorCode::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE
            }
            Self::GraphExists(_) => PgSqlErrorCode::ERRCODE_DUPLICATE_OBJECT,
            Self::RetainVersions(_) => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            Self::TooLarge(_) => PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
            Self::Access(error) => error.sqlstate(),
            Self::Source(error) => error.sqlstate(),
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        }
    }
}
