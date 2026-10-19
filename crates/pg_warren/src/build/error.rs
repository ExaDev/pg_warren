//! The build feature's error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;
use warren_core::graph::GraphError;

use super::source::SourceError;
use crate::catalog::CatalogError;
use crate::extension::SqlError;

/// Why a build could not run.
#[derive(Debug, Error)]
pub(crate) enum BuildError {
    /// The graph's definition could not be found or used.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// A source query does not fit the graph.
    #[error(transparent)]
    Source(#[from] SourceError),
    /// An SPI call failed.
    #[error(transparent)]
    Spi(#[from] pgrx::spi::Error),
    /// A count does not fit its `bigint` column.
    #[error("{0} exceeds the range of bigint")]
    TooLarge(&'static str),
    /// The graph structure could not be built, encoded or decoded.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// A segment of a version has no chunks.
    #[error("version {version} has no segment {segment:?}")]
    SegmentMissing {
        /// The version.
        version: i64,
        /// The segment name.
        segment: String,
    },
    /// A segment's chunks are not numbered `0..n`.
    #[error("segment {segment:?} of version {version} is missing chunk {chunk_no}")]
    ChunkMissing {
        /// The version.
        version: i64,
        /// The segment name.
        segment: String,
        /// The first chunk number that is absent.
        chunk_no: i32,
    },
    /// A segment needs more than 2³¹ chunks.
    #[error("segment {segment:?} needs more than 2^31 chunks")]
    TooManyChunks {
        /// The segment name.
        segment: String,
    },
}

impl SqlError for BuildError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::Catalog(error) => error.sqlstate(),
            Self::Source(error) => error.sqlstate(),
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
            Self::Graph(GraphError::OutOfMemory { .. }) => PgSqlErrorCode::ERRCODE_OUT_OF_MEMORY,
            Self::Graph(
                GraphError::TooManyRelations
                | GraphError::IdSpaceTooLarge
                | GraphError::RelationNameTooLong,
            )
            | Self::TooLarge(_)
            | Self::TooManyChunks { .. } => PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
            Self::Graph(_) | Self::SegmentMissing { .. } | Self::ChunkMissing { .. } => {
                PgSqlErrorCode::ERRCODE_DATA_CORRUPTED
            }
        }
    }
}
