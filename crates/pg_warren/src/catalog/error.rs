//! The catalog feature's error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;

use crate::access::AccessError;
use crate::extension::SqlError;
use crate::source::SourceError;

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
            Self::GraphNotFound(_) => PgSqlErrorCode::ERRCODE_UNDEFINED_OBJECT,
            Self::GraphExists(_) => PgSqlErrorCode::ERRCODE_DUPLICATE_OBJECT,
            Self::RetainVersions(_) => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            Self::TooLarge(_) => PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
            Self::Access(error) => error.sqlstate(),
            Self::Source(error) => error.sqlstate(),
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        }
    }
}
