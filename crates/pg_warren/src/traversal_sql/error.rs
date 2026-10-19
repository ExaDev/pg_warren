//! The traversal bindings' error type, and the one entry point every
//! traversal function's body runs through.

use pgrx::PgSqlErrorCode;
use thiserror::Error;
use warren_core::traversal::TraversalError;

use crate::cache::{CacheError, LoadError};
use crate::extension::{graph_sqlstate, or_raise, SqlError};

/// Why a traversal function could not answer.
#[derive(Debug, Error)]
pub(crate) enum TraversalSqlError {
    /// An argument that must be given was NULL.
    #[error("{0} must not be NULL")]
    NullArgument(&'static str),
    /// An argument has the wrong id type for the graph.
    #[error(
        "graph \"{graph}\" has node ids of type {id_type}, but {argument} is of type {found}; \
         cast it to {cast}"
    )]
    IdType {
        /// The graph.
        graph: String,
        /// The graph's id type.
        id_type: String,
        /// The argument.
        argument: &'static str,
        /// The argument's type.
        found: String,
        /// The type to cast the argument to.
        cast: String,
    },
    /// `direction` is not `out`, `in` or `both`.
    #[error("direction must be 'out', 'in' or 'both', not '{0}'")]
    Direction(String),
    /// A depth or count argument is negative.
    #[error("{0} must not be negative")]
    Negative(&'static str),
    /// The graph or version could not be opened.
    #[error(transparent)]
    Cache(#[from] CacheError),
    /// The traversal itself failed.
    #[error("{0}")]
    Traversal(#[from] TraversalError),
}

impl From<LoadError> for TraversalSqlError {
    fn from(error: LoadError) -> Self {
        Self::Cache(error.into())
    }
}

impl SqlError for TraversalSqlError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::NullArgument(_) => PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED,
            Self::IdType { .. } => PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH,
            Self::Direction(_) | Self::Negative(_) => {
                PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE
            }
            Self::Cache(error) => error.sqlstate(),
            Self::Traversal(error) => traversal_sqlstate(error),
        }
    }
}

fn traversal_sqlstate(error: &TraversalError) -> PgSqlErrorCode {
    match error {
        TraversalError::ReverseCsrMissing => {
            PgSqlErrorCode::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE
        }
        TraversalError::InvalidWeight { .. } => PgSqlErrorCode::ERRCODE_DATA_EXCEPTION,
        TraversalError::Cancelled => PgSqlErrorCode::ERRCODE_QUERY_CANCELED,
        TraversalError::Graph(error) => graph_sqlstate(error),
        _ => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
    }
}

/// Runs the body of a traversal function and returns its result, or
/// raises its error as a SQL `ERROR`. Every `warren.*` traversal function
/// enters here, and set-returning functions also wrap each later fetch.
pub(crate) fn entry<T>(body: impl FnOnce() -> Result<T, TraversalSqlError>) -> T {
    or_raise(body())
}
