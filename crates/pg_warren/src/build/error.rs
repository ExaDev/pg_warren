//! The build feature's error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;
use warren_core::compute::ComputeError;
use warren_core::graph::GraphError;

use crate::access::AccessError;
use crate::access::SqlError;
use crate::cache::LoadError;
use crate::catalog::CatalogError;
use crate::extension::{compute_sqlstate, graph_sqlstate};
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
    /// The version a build starts from could not be loaded.
    #[error(transparent)]
    Load(#[from] LoadError),
    /// An SPI call failed.
    #[error(transparent)]
    Spi(#[from] pgrx::spi::Error),
    /// The graph structure could not be built, encoded or decoded.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// Mapping edge endpoints on the compute pool failed or was cancelled.
    #[error(transparent)]
    Compute(#[from] ComputeError),
    /// A required argument is NULL.
    #[error("{0} must not be NULL")]
    NullArgument(&'static str),
}

impl SqlError for BuildError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::Catalog(error) => error.sqlstate(),
            Self::Access(error) => error.sqlstate(),
            Self::Source(error) => error.sqlstate(),
            Self::Segment(error) => error.sqlstate(),
            Self::Load(error) => error.sqlstate(),
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
            Self::Graph(error) => graph_sqlstate(error),
            Self::Compute(error) => compute_sqlstate(error),
            Self::NullArgument(_) => PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED,
        }
    }
}
