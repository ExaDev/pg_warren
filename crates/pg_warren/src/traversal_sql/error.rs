//! The traversal bindings' error type, and the entry point their bodies
//! run through.

use pgrx::PgSqlErrorCode;
use thiserror::Error;
use warren_core::traversal::TraversalError;

use crate::access::{self, SqlError};
use crate::cache::{CacheError, LoadError};
use crate::extension::graph_sqlstate;

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
    /// A `warren.step` could not be made or read.
    #[error("invalid warren.step: {0}")]
    Step(String),
    /// `mode` is not `ends`, `count` or `paths`.
    #[error("mode must be 'ends', 'count' or 'paths', not '{0}'")]
    Mode(String),
    /// `max_results` is above the `warren.max_walk_paths` ceiling.
    #[error(
        "max_results {requested} is above warren.max_walk_paths ({ceiling}) for a walk that \
         reports a row per walk"
    )]
    WalkRowCeiling {
        /// The `max_results` asked for.
        requested: i64,
        /// The ceiling.
        ceiling: i64,
    },
    /// A walk count does not fit `bigint`.
    #[error("walk count {0} exceeds the range of bigint")]
    CountOutOfRange(u64),
    /// An argument that must be an array is not one.
    #[error("{argument} must be an array (e.g. ARRAY[...]::{id_type}[]), not {found}")]
    NotAnArray {
        /// The argument.
        argument: &'static str,
        /// The id type to make an array of.
        id_type: String,
        /// The argument's type.
        found: String,
    },
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
            Self::IdType { .. } | Self::NotAnArray { .. } => {
                PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH
            }
            Self::CountOutOfRange(_) => PgSqlErrorCode::ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE,
            Self::WalkRowCeiling { .. } => PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
            Self::Direction(_) | Self::Negative(_) | Self::Step(_) | Self::Mode(_) => {
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
        TraversalError::InvalidHopRange { .. } => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
        TraversalError::WalkCountOverflow => PgSqlErrorCode::ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE,
        TraversalError::Cancelled => PgSqlErrorCode::ERRCODE_QUERY_CANCELED,
        TraversalError::Graph(error) => graph_sqlstate(error),
        _ => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
    }
}

/// Runs the body of a traversal function under the access guard (trusted
/// `search_path`, errors raised with their SQLSTATE) and returns its
/// result. Every traversal function enters here, and so does each later
/// fetch of a set-returning one.
pub(crate) fn entry<T>(body: impl FnOnce() -> Result<T, TraversalSqlError>) -> T {
    access::entry(|_| body())
}
