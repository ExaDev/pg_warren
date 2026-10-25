//! The cache feature's error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;

use super::LoadError;
use crate::access::{AccessError, SqlError};
use crate::catalog::{CatalogError, VersionId};

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
        "cache memory limit exceeded: a transient vector of {requested} bytes would bring \
         transient vectors to {after} bytes, over warren.max_cache_memory ({limit} bytes); \
         {in_use} are in use, {own} of them this role's; persist it, drop transient vectors or \
         raise warren.max_cache_memory"
    )]
    TransientOverLimit {
        /// The vector's bytes.
        requested: u64,
        /// The bytes transient vectors would hold with it.
        after: u64,
        /// The bytes transient vectors hold, of every role.
        in_use: u64,
        /// The bytes the current role's transient vectors hold.
        own: u64,
        /// The cap, in bytes.
        limit: u64,
    },
    /// Loading versions would take what the statement holds in the cache,
    /// which cannot be evicted until it ends, past `warren.max_cache_memory`
    /// and `warren.max_compute_memory` together.
    #[error(
        "memory limit exceeded: loading {} ({expected} bytes) would bring what this statement \
         holds in the cache to {} bytes, over warren.max_cache_memory ({cache_limit} bytes) \
         and warren.max_compute_memory ({compute_limit} bytes) together; {held} bytes are held \
         already, by transient vectors and what the statement has read; raise \
         warren.max_cache_memory or warren.max_compute_memory",
        versions_named(versions),
        held.saturating_add(*expected)
    )]
    StatementOverLimit {
        /// The versions to load.
        versions: Vec<VersionId>,
        /// About the bytes they would add.
        expected: u64,
        /// The bytes the cache holds that it cannot evict.
        held: u64,
        /// `warren.max_cache_memory`, in bytes.
        cache_limit: u64,
        /// `warren.max_compute_memory`, in bytes.
        compute_limit: u64,
    },
    /// An SPI call failed.
    #[error(transparent)]
    Spi(#[from] pgrx::spi::Error),
    /// A required argument is NULL.
    #[error("{0} must not be NULL")]
    NullArgument(&'static str),
}

impl SqlError for CacheError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::NoCurrentVersion(_) => PgSqlErrorCode::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE,
            Self::Catalog(error) => error.sqlstate(),
            Self::Access(error) => error.sqlstate(),
            Self::Load(error) => error.sqlstate(),
            Self::TransientOverLimit { .. } | Self::StatementOverLimit { .. } => {
                PgSqlErrorCode::ERRCODE_CONFIGURATION_LIMIT_EXCEEDED
            }
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
            Self::NullArgument(_) => PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED,
        }
    }
}

/// `version 3`, or `versions 3 and 4`.
fn versions_named(versions: &[VersionId]) -> String {
    let named: Vec<String> = versions.iter().map(ToString::to_string).collect();
    match named.split_last() {
        Some((last, [])) => format!("version {last}"),
        Some((last, rest)) => format!("versions {} and {last}", rest.join(", ")),
        None => "no version".to_owned(),
    }
}
