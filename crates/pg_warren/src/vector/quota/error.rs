//! The quota feature's error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;

use crate::access::{AccessError, SqlError};
use crate::catalog::CatalogError;

/// Why a quota could not be set or read, or a write was refused.
#[derive(Debug, Error)]
pub(crate) enum QuotaError {
    /// The write would take the role's persisted vectors on the graph past
    /// its quota, and above what they were (DESIGN.md §14 item 47).
    #[error(
        "vector \"{vector}\" ({written} bytes) would take the persisted vectors of role \
         \"{role}\" on graph \"{graph}\" to {after} bytes, past its quota of {quota} bytes (it \
         stores {before}); drop some of its vectors, write it with persist := false, or ask the \
         graph's owner for a larger quota"
    )]
    Exceeded {
        /// The vector written.
        vector: String,
        /// Its bytes.
        written: u64,
        /// The writing role.
        role: String,
        /// The graph.
        graph: String,
        /// The role's quota, in bytes.
        quota: u64,
        /// What it stores before the write.
        before: u64,
        /// What it would store after it.
        after: u64,
    },
    /// A quota is a number of bytes, not negative.
    #[error("a vector quota cannot be negative: {0} bytes")]
    Negative(i64),
    /// `InvalidOid` (`'-'::regrole`) as the role: NULL means the default.
    #[error("a vector quota is set for a role, not for InvalidOid ('-'); pass NULL for the graph's default")]
    InvalidRole,
    /// A NULL where a value is required.
    #[error("{0} must not be NULL")]
    NullArgument(&'static str),
    /// The graph was not found, or its row lock failed.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// The caller may not do this, or an identity switch failed.
    #[error(transparent)]
    Access(#[from] AccessError),
    /// An SPI call failed.
    #[error(transparent)]
    Spi(#[from] pgrx::spi::Error),
}

impl SqlError for QuotaError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::Exceeded { .. } => PgSqlErrorCode::ERRCODE_CONFIGURATION_LIMIT_EXCEEDED,
            Self::Negative(_) | Self::InvalidRole => {
                PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE
            }
            Self::NullArgument(_) => PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED,
            Self::Catalog(error) => error.sqlstate(),
            Self::Access(error) => error.sqlstate(),
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        }
    }
}
