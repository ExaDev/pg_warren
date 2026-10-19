//! The build feature's error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;
use warren_core::graph::GraphError;

use crate::access::AccessError;
use crate::catalog::CatalogError;
use crate::extension::SqlError;
use crate::segment::SegmentError;
use crate::source::SourceError;

/// Why a build could not run.
#[derive(Debug, Error)]
pub(crate) enum BuildError {
    /// The graph's definition or versions could not be read or written.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// The caller may not refresh the graph, or an identity switch failed.
    #[error(transparent)]
    Access(#[from] AccessError),
    /// A source query does not fit the graph.
    #[error(transparent)]
    Source(#[from] SourceError),
    /// A segment could not be stored or read.
    #[error(transparent)]
    Segment(#[from] SegmentError),
    /// An SPI call failed.
    #[error(transparent)]
    Spi(#[from] pgrx::spi::Error),
    /// The graph structure could not be built, encoded or decoded.
    #[error(transparent)]
    Graph(#[from] GraphError),
}

impl SqlError for BuildError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::Catalog(error) => error.sqlstate(),
            Self::Access(error) => error.sqlstate(),
            Self::Source(error) => error.sqlstate(),
            Self::Segment(error) => error.sqlstate(),
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
            Self::Graph(error) => graph_sqlstate(error),
        }
    }
}

/// The SQLSTATE of a graph error: out of memory, a size limit, or (when
/// decoding) corrupt data.
pub(crate) fn graph_sqlstate(error: &GraphError) -> PgSqlErrorCode {
    match error {
        GraphError::OutOfMemory { .. } => PgSqlErrorCode::ERRCODE_OUT_OF_MEMORY,
        GraphError::TooManyRelations
        | GraphError::IdSpaceTooLarge
        | GraphError::RelationNameTooLong => PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
        _ => PgSqlErrorCode::ERRCODE_DATA_CORRUPTED,
    }
}
