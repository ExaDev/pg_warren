//! Why an id argument does not fit the graph, or a call's ids could not
//! be looked up.

use pgrx::PgSqlErrorCode;
use thiserror::Error;
use warren_core::graph::GraphError;

use crate::access::SqlError;
use crate::extension::graph_sqlstate;

/// Why an id argument does not fit the graph, or a call's ids could not be
/// looked up.
#[derive(Debug, Error)]
pub(crate) enum ExternalIdError {
    /// An id argument that must be given was NULL.
    #[error("{0} must not be NULL")]
    Null(&'static str),
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
    /// A call's hash index of the graph's ids could not be built.
    #[error(transparent)]
    Index(#[from] GraphError),
}

impl SqlError for ExternalIdError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::Null(_) => PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED,
            Self::IdType { .. } | Self::NotAnArray { .. } => {
                PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH
            }
            Self::Index(error) => graph_sqlstate(error),
        }
    }
}
