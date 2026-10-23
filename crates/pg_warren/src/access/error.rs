//! The access feature's error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;

use super::SqlError;

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
    /// A persisted vector was written by another role, of which the caller
    /// is not a member, and the caller is not a member of the owner role.
    #[error(
        "vector \"{vector}\" of graph \"{graph}\" was written by role \"{creator}\"; only that \
         role or members of role \"{owner}\" may replace or drop it"
    )]
    NotCreator {
        /// The vector.
        vector: String,
        /// The graph.
        graph: String,
        /// The role that wrote it.
        creator: String,
        /// The owner role's name.
        owner: String,
    },
    /// A persisted vector was written by a role since dropped, and the
    /// caller is not a member of the owner role.
    #[error(
        "vector \"{vector}\" of graph \"{graph}\" was written by a role that no longer exists; \
         only members of role \"{owner}\" may replace or drop it"
    )]
    CreatorDropped {
        /// The vector.
        vector: String,
        /// The graph.
        graph: String,
        /// The owner role's name.
        owner: String,
    },
    /// The caller may not `SET ROLE` to the role it would make a graph's
    /// owner (DESIGN.md §5.7).
    #[error("must be able to SET ROLE \"{role}\" to make it the owner of graph \"{graph}\"")]
    CannotBecome {
        /// The would-be owner's name.
        role: String,
        /// The graph.
        graph: String,
    },
    /// The caller lacks the privileges of a role whose graphs and vectors
    /// it would reassign or drop, as `REASSIGN OWNED` and `DROP OWNED`
    /// require them (DESIGN.md §5.7).
    #[error("must have the privileges of role \"{role}\" to {action}")]
    NoPrivilegesOf {
        /// The role's name.
        role: String,
        /// What the caller tried to do.
        action: &'static str,
    },
    /// The caller may not `SET ROLE` to the role it would give another
    /// role's graphs and vectors (DESIGN.md §5.7).
    #[error(
        "must be able to SET ROLE \"{role}\" to give it the graphs and vectors of role \"{from}\""
    )]
    CannotReceive {
        /// The would-be owner's name.
        role: String,
        /// The role whose graphs and vectors it would receive.
        from: String,
    },
    /// Only a superuser or a member of the extension's owner may drop a
    /// graph whose owner was dropped (DESIGN.md §14 item 45).
    #[error(
        "must be a member of role \"{0}\", the owner of extension pg_warren, to drop graphs whose \
         owner no longer exists"
    )]
    NotOrphanDropper(String),
    /// Only a superuser may give a graph whose owner was dropped a new
    /// owner (DESIGN.md §14 item 45).
    #[error("only a superuser may give graph \"{0}\", whose owner no longer exists, a new owner")]
    AdoptOrphan(String),
    /// What the bootstrap superuser owns is required by the database
    /// system, so `DROP OWNED` and `REASSIGN OWNED` refuse it.
    #[error(
        "cannot {action} objects owned by role \"{role}\" because they are required by the \
         database system"
    )]
    RequiredBySystem {
        /// The bootstrap superuser's name.
        role: String,
        /// What the caller tried to do.
        action: &'static str,
    },
    /// No role has this oid.
    #[error("role with OID {0} does not exist")]
    RoleNotFound(u32),
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
            Self::NotOwner { .. }
            | Self::NotReader { .. }
            | Self::NotCreator { .. }
            | Self::CreatorDropped { .. }
            | Self::CannotBecome { .. }
            | Self::NoPrivilegesOf { .. }
            | Self::CannotReceive { .. }
            | Self::NotOrphanDropper(_)
            | Self::AdoptOrphan(_) => PgSqlErrorCode::ERRCODE_INSUFFICIENT_PRIVILEGE,
            Self::OwnerDropped(_) | Self::RoleNotFound(_) | Self::ExtensionMissing => {
                PgSqlErrorCode::ERRCODE_UNDEFINED_OBJECT
            }
            Self::RequiredBySystem { .. } => PgSqlErrorCode::ERRCODE_DEPENDENT_OBJECTS_STILL_EXIST,
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        }
    }
}
