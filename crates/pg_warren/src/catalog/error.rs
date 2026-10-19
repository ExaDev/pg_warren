//! The catalog feature's error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;

use crate::build::SourceError;
use crate::extension::SqlError;

/// Why a graph definition could not be created.
#[derive(Debug, Error)]
pub(crate) enum CatalogError {
    /// A graph with this name exists already.
    #[error("graph \"{0}\" already exists")]
    GraphExists(String),
    /// `retain_versions` must keep at least the current version.
    #[error("retain_versions must be at least 1, not {0}")]
    RetainVersions(i32),
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
            Self::ExtensionMissing => PgSqlErrorCode::ERRCODE_UNDEFINED_OBJECT,
            Self::GraphExists(_) => PgSqlErrorCode::ERRCODE_DUPLICATE_OBJECT,
            Self::RetainVersions(_) => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            Self::Source(error) => error.sqlstate(),
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        }
    }
}
