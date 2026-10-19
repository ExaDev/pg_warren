//! The access feature's error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;

use crate::extension::SqlError;

/// Why the caller may not do what it asked, or an identity switch failed.
#[derive(Debug, Error)]
pub(crate) enum AccessError {
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
    /// The caller is neither a member of the owner role nor of a reader.
    #[error(
        "must be a member of role \"{owner}\" or of a reader role to {action} graph \"{graph}\""
    )]
    NotReader {
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
    /// An SPI call failed.
    #[error(transparent)]
    Spi(#[from] pgrx::spi::Error),
}

impl SqlError for AccessError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::NotOwner { .. } | Self::NotReader { .. } => {
                PgSqlErrorCode::ERRCODE_INSUFFICIENT_PRIVILEGE
            }
            Self::OwnerDropped(_) | Self::ExtensionMissing => {
                PgSqlErrorCode::ERRCODE_UNDEFINED_OBJECT
            }
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        }
    }
}
