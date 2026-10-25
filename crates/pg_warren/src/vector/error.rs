//! The vector feature's error type.

use pgrx::PgSqlErrorCode;
use thiserror::Error;
use warren_core::compute::ComputeError;

use super::quota::QuotaError;
use super::VectorId;
use crate::access::{AccessError, SqlError};
use crate::catalog::VersionId;

/// Why a vector could not be stored or read.
#[derive(Debug, Error)]
pub(crate) enum VectorError {
    /// A vector's chunks are not numbered `0..n`.
    #[error("vector {vector} is missing chunk {chunk_no}")]
    ChunkMissing {
        /// The vector.
        vector: VectorId,
        /// The first chunk number that is absent.
        chunk_no: i32,
    },
    /// A vector needs more than 2³¹ chunks.
    #[error("vector \"{0}\" needs more than 2^31 chunks")]
    TooManyChunks(String),
    /// A concurrent transaction wrote a vector of this name meanwhile.
    #[error(
        "vector \"{name}\" at version {version} was written by a concurrent transaction; retry"
    )]
    WrittenConcurrently {
        /// The vector's name.
        name: String,
        /// The version.
        version: VersionId,
    },
    /// The version was deleted (by retention or `drop_version`) after the
    /// writer opened it (DESIGN.md §7).
    #[error("version {version} no longer exists: vector \"{name}\" was not written to it")]
    VersionGone {
        /// The vector's name.
        name: String,
        /// The version.
        version: VersionId,
    },
    /// A stored dtype is not one this library knows.
    #[error("vector {vector} has an unknown dtype \"{dtype}\"")]
    UnknownDtype {
        /// The vector.
        vector: VectorId,
        /// The stored dtype.
        dtype: String,
    },
    /// The vector's bytes could not be encoded or decoded, or memory ran
    /// out.
    #[error(transparent)]
    Compute(#[from] ComputeError),
    /// A quota refused the write, or could not be read (DESIGN.md §5.7).
    #[error(transparent)]
    Quota(#[from] QuotaError),
    /// The switch to the catalog owner failed.
    #[error(transparent)]
    Access(#[from] AccessError),
    /// An SPI call failed.
    #[error(transparent)]
    Spi(#[from] pgrx::spi::Error),
}

impl SqlError for VectorError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::ChunkMissing { .. } | Self::UnknownDtype { .. } => {
                PgSqlErrorCode::ERRCODE_DATA_CORRUPTED
            }
            Self::TooManyChunks(_) => PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
            Self::WrittenConcurrently { .. } => PgSqlErrorCode::ERRCODE_T_R_SERIALIZATION_FAILURE,
            Self::VersionGone { .. } => PgSqlErrorCode::ERRCODE_UNDEFINED_OBJECT,
            Self::Compute(ComputeError::MemoryLimit { .. }) => {
                PgSqlErrorCode::ERRCODE_CONFIGURATION_LIMIT_EXCEEDED
            }
            Self::Compute(ComputeError::OutOfMemory { .. }) => {
                PgSqlErrorCode::ERRCODE_OUT_OF_MEMORY
            }
            Self::Compute(_) => PgSqlErrorCode::ERRCODE_DATA_CORRUPTED,
            Self::Quota(error) => error.sqlstate(),
            Self::Access(error) => error.sqlstate(),
            Self::Spi(_) => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        }
    }
}
