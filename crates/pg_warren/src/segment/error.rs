//! The segment feature's error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;
use warren_core::graph::GraphError;

use crate::access::AccessError;
use crate::access::SqlError;
use crate::catalog::VersionId;
use crate::extension::graph_sqlstate;

/// Why a segment could not be stored or read.
#[derive(Debug, Error)]
pub(crate) enum SegmentError {
    /// A segment of a version has no chunks.
    #[error("version {version} has no segment {segment:?}")]
    SegmentMissing {
        /// The version.
        version: VersionId,
        /// The segment name.
        segment: String,
    },
    /// A segment's chunks are not numbered `0..n`.
    #[error("segment {segment:?} of version {version} is missing chunk {chunk_no}")]
    ChunkMissing {
        /// The version.
        version: VersionId,
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
    /// A segment could not be encoded or decoded.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// The switch to the catalog owner failed.
    #[error(transparent)]
    Access(#[from] AccessError),
    /// An SPI call failed.
    #[error(transparent)]
    Spi(#[from] pgrx::spi::Error),
}

impl SqlError for SegmentError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::SegmentMissing { .. } | Self::ChunkMissing { .. } => {
                PgSqlErrorCode::ERRCODE_DATA_CORRUPTED
            }
            Self::TooManyChunks { .. } => PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
            Self::Graph(error) => graph_sqlstate(error),
            Self::Access(error) => error.sqlstate(),
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        }
    }
}
