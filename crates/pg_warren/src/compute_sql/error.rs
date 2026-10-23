//! The compute bindings' error type, and the entry point their bodies run
//! through.

use pgrx::PgSqlErrorCode;
use thiserror::Error;
use warren_core::compute::ComputeError;
use warren_core::version::VersionError;

use crate::access::{self, AccessError, SqlError};
use crate::cache::{CacheError, LoadError};
use crate::catalog::{CatalogError, VersionId};
use crate::extension::compute_sqlstate;
use crate::external_ids::ExternalIdError;
use crate::selection::SelectionError;
use crate::vector::VectorError;

/// Why a compute function could not answer.
#[derive(Debug, Error)]
pub(crate) enum ComputeSqlError {
    /// An argument that must be given was NULL.
    #[error("{0} must not be NULL")]
    NullArgument(&'static str),
    /// No vector of this name is bound to the version.
    #[error("graph \"{graph}\" has no vector \"{name}\" at version {version}")]
    VectorNotFound {
        /// The graph.
        graph: String,
        /// The vector name.
        name: String,
        /// The version.
        version: VersionId,
    },
    /// `vector_from_query`'s query returned two rows for one node.
    #[error("query returns more than one row for node {0}")]
    DuplicateNode(String),
    /// A `vector_map` expression is not one SQL expression.
    #[error("{0}")]
    MapSyntax(String),
    /// A `vector_map` expression uses SQL no vector can be computed with.
    #[error("{0}")]
    MapUnsupported(&'static str),
    /// A `vector_map` expression's type is not one a vector holds.
    #[error(
        "a vector_map expression must be of type double precision, real, bigint, integer, \
         smallint, numeric or boolean, not {0}"
    )]
    MapType(String),
    /// `vector_from_query`'s id column does not cast implicitly to the
    /// graph's id type.
    #[error(
        "query column 1 (id) is of type {found}, which does not cast implicitly to the graph's \
         id type {id_type}"
    )]
    IdColumnType {
        /// The column's type.
        found: String,
        /// The graph's id type.
        id_type: String,
    },
    /// `gather`'s combiner does not accept the messages' type.
    #[error(transparent)]
    GatherType(#[from] super::gather::TypeError),
    /// `gather`'s `value_type` witness is not the combiner's result type.
    #[error(
        "gather with combine := {combiner} over {message} messages returns {result}: pass \
         value_type := NULL::{result}, not {found}"
    )]
    ValueType {
        /// The combiner.
        combiner: warren_core::compute::Combiner,
        /// The messages' type.
        message: String,
        /// The result type.
        result: String,
        /// The witness's type.
        found: String,
    },
    /// `gather`'s `messages` names both a vector and a relation.
    #[error(
        "messages \"{name}\" names both a vector bound to version {version} and a relation: \
         write TABLE {name} for the relation, or rename the vector"
    )]
    AmbiguousMessages {
        /// The name.
        name: String,
        /// The version the vector is bound to.
        version: VersionId,
    },
    /// Persisted vector names, and the library's arguments, must not use
    /// the prefix of its intermediates.
    #[error(
        "vector names beginning with _warren_ are reserved for the library's intermediates, and \
         are only ever transient: \"{0}\""
    )]
    ReservedName(String),
    /// A vector name must not be empty.
    #[error("a vector name must not be empty")]
    EmptyName,
    /// An argument is out of its range.
    #[error("{0}")]
    InvalidArgument(String),
    /// The caller may not do this.
    #[error(transparent)]
    Access(#[from] AccessError),
    /// The graph or version does not exist.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// The graph or version could not be opened.
    #[error(transparent)]
    Cache(#[from] CacheError),
    /// A vector could not be stored or read.
    #[error(transparent)]
    Vector(#[from] VectorError),
    /// An id argument did not fit the graph.
    #[error(transparent)]
    ExternalId(#[from] ExternalIdError),
    /// The direction or relations could not be selected.
    #[error(transparent)]
    Selection(#[from] SelectionError),
    /// A query does not fit.
    #[error(transparent)]
    Source(#[from] crate::source::SourceError),
    /// The kernel failed.
    #[error("{0}")]
    Compute(#[from] ComputeError),
    /// Two versions do not share an id space.
    #[error(transparent)]
    Version(#[from] VersionError),
    /// An SPI call failed.
    #[error(transparent)]
    Spi(#[from] pgrx::spi::Error),
}

impl From<LoadError> for ComputeSqlError {
    fn from(error: LoadError) -> Self {
        Self::Cache(error.into())
    }
}

impl SqlError for ComputeSqlError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::NullArgument(_) => PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED,
            Self::VectorNotFound { .. } => PgSqlErrorCode::ERRCODE_UNDEFINED_OBJECT,
            Self::DuplicateNode(_) => PgSqlErrorCode::ERRCODE_CARDINALITY_VIOLATION,
            Self::IdColumnType { .. } => PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH,
            Self::MapSyntax(_) => PgSqlErrorCode::ERRCODE_SYNTAX_ERROR,
            Self::MapUnsupported(_) => PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
            Self::MapType(_) | Self::GatherType(_) | Self::ValueType { .. } => {
                PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH
            }
            Self::AmbiguousMessages { .. } => PgSqlErrorCode::ERRCODE_AMBIGUOUS_FUNCTION,
            Self::ReservedName(_) => PgSqlErrorCode::ERRCODE_RESERVED_NAME,
            Self::EmptyName | Self::InvalidArgument(_) | Self::Version(_) => {
                PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE
            }
            Self::Access(error) => error.sqlstate(),
            Self::Catalog(error) => error.sqlstate(),
            Self::Cache(error) => error.sqlstate(),
            Self::Vector(error) => error.sqlstate(),
            Self::ExternalId(error) => error.sqlstate(),
            Self::Selection(error) => error.sqlstate(),
            Self::Source(error) => error.sqlstate(),
            Self::Compute(error) => compute_sqlstate(error),
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        }
    }
}

/// Runs the body of a compute function under the access guard (trusted
/// `search_path`, errors raised with their SQLSTATE).
pub(crate) fn entry<T>(body: impl FnOnce(&access::Caller) -> Result<T, ComputeSqlError>) -> T {
    access::entry(body)
}
