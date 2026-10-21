//! The catalog feature's error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;

use super::VersionId;
use crate::access::AccessError;
use crate::access::SqlError;
use crate::extension::graph_sqlstate;
use crate::source::SourceError;
use warren_core::graph::GraphError;

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
    /// The recorded id type is not one this library supports.
    #[error("graph \"{0}\" has an unsupported id type")]
    UnknownIdType(String),
    /// A count does not fit its `bigint` column.
    #[error("{0} exceeds the range of bigint")]
    TooLarge(&'static str),
    /// The caller may not do this, or an identity switch failed.
    #[error(transparent)]
    Access(#[from] AccessError),
    /// A source query does not fit the graph.
    #[error(transparent)]
    Source(#[from] SourceError),
    /// `graphs.current_version` names a version that is not one of the
    /// graph's, as a restored or edited row can (DESIGN.md §5.1).
    #[error(
        "graph \"{graph}\" records current version {version}, which is not one of its versions \
         (a restored or edited catalog row); promote one of its versions"
    )]
    StrayCurrentVersion {
        /// The graph.
        graph: String,
        /// The version its row names.
        version: i64,
    },
    /// A node order other than 'id' or 'locality' (DESIGN.md §14 item 22).
    #[error(transparent)]
    NodeOrder(GraphError),
    /// A graph derived by contraction gets new versions only from
    /// `warren.contract` (DESIGN.md §6.6).
    #[error("cannot {action} graph \"{graph}\": it is derived by contraction; warren.contract makes its versions")]
    DerivedByContraction {
        /// The graph.
        graph: String,
        /// What was refused.
        action: &'static str,
    },
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
            Self::SupersededIdSpace { .. } | Self::StrayCurrentVersion { .. } => {
                PgSqlErrorCode::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE
            }
            Self::GraphExists(_) => PgSqlErrorCode::ERRCODE_DUPLICATE_OBJECT,
            Self::RetainVersions(_) => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            Self::UnknownIdType(_) => PgSqlErrorCode::ERRCODE_DATA_CORRUPTED,
            Self::TooLarge(_) => PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
            Self::Access(error) => error.sqlstate(),
            Self::Source(error) => error.sqlstate(),
            Self::NodeOrder(error) => graph_sqlstate(error),
            Self::DerivedByContraction { .. } => PgSqlErrorCode::ERRCODE_WRONG_OBJECT_TYPE,
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        }
    }
}
