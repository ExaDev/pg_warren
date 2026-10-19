//! The cache feature's error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;

use super::LoadError;
use crate::access::{AccessError, SqlError};
use crate::catalog::CatalogError;

/// Why a version could not be opened for reading.
#[derive(Debug, Error)]
pub(crate) enum CacheError {
    /// The graph has never been promoted and no version was asked for.
    #[error("graph \"{0}\" has no current version; refresh it, or pass a version")]
    NoCurrentVersion(String),
    /// The graph or version does not exist.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// The caller may not read the graph.
    #[error(transparent)]
    Access(#[from] AccessError),
    /// The version could not be loaded.
    #[error(transparent)]
    Load(#[from] LoadError),
    /// Keeping a transient vector would take transient vectors past
    /// `warren.max_cache_memory`.
    #[error(
        "cache memory limit exceeded: a transient vector of {requested} bytes requested, \
         {in_use} of {limit} in use by transient vectors; persist it, drop transient vectors \
         or raise warren.max_cache_memory"
    )]
    TransientOverLimit {
        /// The vector's bytes.
        requested: u64,
        /// The bytes transient vectors hold.
        in_use: u64,
        /// The cap, in bytes.
        limit: u64,
    },
    /// An SPI call failed.
    #[error(transparent)]
    Spi(#[from] pgrx::spi::Error),
}

impl SqlError for CacheError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::NoCurrentVersion(_) => PgSqlErrorCode::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE,
            Self::Catalog(error) => error.sqlstate(),
            Self::Access(error) => error.sqlstate(),
            Self::Load(error) => error.sqlstate(),
            Self::TransientOverLimit { .. } => PgSqlErrorCode::ERRCODE_CONFIGURATION_LIMIT_EXCEEDED,
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        }
    }
}
