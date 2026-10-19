//! The compute bindings' error type, and the entry point their bodies run
//! through.

use pgrx::PgSqlErrorCode;
use thiserror::Error;
use warren_core::compute::{ArithmeticError, ComputeError};

use crate::access::{self, AccessError, SqlError};
use crate::cache::{CacheError, LoadError};
use crate::catalog::{CatalogError, VersionId};
use crate::traversal_sql::TraversalSqlError;
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
    /// An id argument or relation did not fit the graph.
    #[error(transparent)]
    Traversal(#[from] TraversalSqlError),
    /// A query does not fit.
    #[error(transparent)]
    Source(#[from] crate::source::SourceError),
    /// The kernel failed.
    #[error("{0}")]
    Compute(#[from] ComputeError),
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
            Self::EmptyName | Self::InvalidArgument(_) => {
                PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE
            }
            Self::Access(error) => error.sqlstate(),
            Self::Catalog(error) => error.sqlstate(),
            Self::Cache(error) => error.sqlstate(),
            Self::Vector(error) => error.sqlstate(),
            Self::Traversal(error) => error.sqlstate(),
            Self::Source(error) => error.sqlstate(),
            Self::Compute(error) => compute_sqlstate(error),
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        }
    }
}

/// The SQLSTATE of a kernel error: the one Postgres raises for the same
/// arithmetic, so the compiled path and the executor agree.
pub(crate) fn compute_sqlstate(error: &ComputeError) -> PgSqlErrorCode {
    match error {
        ComputeError::MemoryLimit { .. } => PgSqlErrorCode::ERRCODE_CONFIGURATION_LIMIT_EXCEEDED,
        ComputeError::OutOfMemory { .. } => PgSqlErrorCode::ERRCODE_OUT_OF_MEMORY,
        ComputeError::UnknownDtype(_)
        | ComputeError::UnknownSemiring(_)
        | ComputeError::UnknownBinaryOp(_)
        | ComputeError::UnknownReduction(_)
        | ComputeError::UnknownNorm(_)
        | ComputeError::ZeroWidth
        | ComputeError::WidthMismatch { .. } => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
        ComputeError::DtypeMismatch { .. } | ComputeError::OperandType { .. } => {
            PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH
        }
        ComputeError::IdSpaceMismatch { .. } => {
            PgSqlErrorCode::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE
        }
        ComputeError::NullResult { .. } => PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED,
        ComputeError::Arithmetic(error) => arithmetic_sqlstate(*error),
        ComputeError::Cancelled => PgSqlErrorCode::ERRCODE_QUERY_CANCELED,
        ComputeError::VectorBytes { .. } | ComputeError::VectorLength { .. } => {
            PgSqlErrorCode::ERRCODE_DATA_CORRUPTED
        }
        _ => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
    }
}

/// The SQLSTATE `float.c` and `int8.c` raise for an arithmetic error.
fn arithmetic_sqlstate(error: ArithmeticError) -> PgSqlErrorCode {
    match error {
        ArithmeticError::BigintOutOfRange
        | ArithmeticError::FloatOverflow
        | ArithmeticError::FloatUnderflow => PgSqlErrorCode::ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE,
        ArithmeticError::DivisionByZero => PgSqlErrorCode::ERRCODE_DIVISION_BY_ZERO,
        ArithmeticError::SqrtOfNegative
        | ArithmeticError::ZeroToNegativePower
        | ArithmeticError::ComplexPower => {
            PgSqlErrorCode::ERRCODE_INVALID_ARGUMENT_FOR_POWER_FUNCTION
        }
        ArithmeticError::LogOfZero | ArithmeticError::LogOfNegative => {
            PgSqlErrorCode::ERRCODE_INVALID_ARGUMENT_FOR_LOG
        }
    }
}

/// Runs the body of a compute function under the access guard (trusted
/// `search_path`, errors raised with their SQLSTATE).
pub(crate) fn entry<T>(body: impl FnOnce(&access::Caller) -> Result<T, ComputeSqlError>) -> T {
    access::entry(body)
}
