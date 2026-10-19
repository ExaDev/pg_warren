//! The catalog feature's error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;

use crate::build::SourceError;
use crate::extension::SqlError;

/// Why a graph definition could not be created, found or used.
#[derive(Debug, Error)]
pub(crate) enum CatalogError {
    /// No graph has this name.
    #[error("graph \"{0}\" does not exist")]
    GraphNotFound(String),
    /// A graph with this name exists already.
    #[error("graph \"{0}\" already exists")]
    GraphExists(String),
    /// `retain_versions` must keep at least the current version.
    #[error("retain_versions must be at least 1, not {0}")]
    RetainVersions(i32),
    /// The caller is not a member of the graph's owner role.
    #[error("must be a member of role \"{owner}\" to {action} graph \"{graph}\"")]
    NotOwner {
        /// The owner role's name.
        owner: String,
        /// What the caller tried to do.
        action: &'static str,
        /// The graph.
        graph: String,
    },
    /// The graph's owner role was dropped (DESIGN.md §7).
    #[error("the owner of graph \"{0}\" no longer exists")]
    OwnerDropped(String),
    /// The pg_warren extension row is missing.
    #[error("extension pg_warren is not installed")]
    ExtensionMissing,
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
            Self::GraphNotFound(_) | Self::OwnerDropped(_) | Self::ExtensionMissing => {
                PgSqlErrorCode::ERRCODE_UNDEFINED_OBJECT
            }
            Self::GraphExists(_) => PgSqlErrorCode::ERRCODE_DUPLICATE_OBJECT,
            Self::RetainVersions(_) => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            Self::NotOwner { .. } => PgSqlErrorCode::ERRCODE_INSUFFICIENT_PRIVILEGE,
            Self::Source(error) => error.sqlstate(),
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        }
    }
}
