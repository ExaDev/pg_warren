//! Why a region could not be allocated or read.

use pgrx::PgSqlErrorCode;
use thiserror::Error;

use crate::access::SqlError;
use crate::catalog::VersionId;

/// Why a region could not be allocated, or does not hold a loaded version
/// as its layout says.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub(crate) enum RegionError {
    /// The allocator could not provide the region.
    #[error("out of memory allocating a region of {bytes} bytes for a loaded version")]
    OutOfMemory {
        /// The bytes requested.
        bytes: usize,
    },
    /// The region's size does not fit in the address space.
    #[error("a loaded version's region does not fit in the address space")]
    TooLarge,
    /// The region, or an array in it, does not start at the alignment its
    /// elements need.
    #[error("region memory at address {address:#x} is misaligned")]
    Misaligned {
        /// The address.
        address: usize,
    },
    /// Bytes that are not a whole number of elements.
    #[error("{bytes} bytes are not a whole number of {width}-byte elements")]
    Ragged {
        /// The bytes.
        bytes: usize,
        /// The element width.
        width: usize,
    },
    /// The region is shorter than its layout.
    #[error("a loaded version's region holds {found} bytes, its layout {needed}")]
    Short {
        /// The bytes its layout needs.
        needed: u64,
        /// The bytes it holds.
        found: u64,
    },
    /// The region does not start with a region header.
    #[error("memory does not hold a loaded version's region")]
    Magic,
    /// The region was written in another format.
    #[error("a loaded version's region has format {found}, not {}", super::layout::FORMAT)]
    Format {
        /// Its format.
        found: u32,
    },
    /// The region holds another version.
    #[error("a region of version {found} was read as version {expected}")]
    Version {
        /// The version expected.
        expected: VersionId,
        /// The version it holds.
        found: VersionId,
    },
    /// An entry of the table of contents names no array, an array that is
    /// misaligned or outside the region, or one an earlier entry names.
    #[error("entry {index} of a loaded version's region is invalid")]
    Entry {
        /// The entry's index.
        index: usize,
    },
    /// An array is viewed as elements of another width.
    #[error("an array of {expected}-byte elements was viewed as {found}-byte elements")]
    ElementWidth {
        /// The width of its elements.
        expected: usize,
        /// The width it was viewed as.
        found: usize,
    },
    /// The region lacks an array every loaded version has.
    #[error("a loaded version's region has no {0}")]
    MissingArray(&'static str),
}

impl SqlError for RegionError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::OutOfMemory { .. } => PgSqlErrorCode::ERRCODE_OUT_OF_MEMORY,
            Self::TooLarge => PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
            Self::Misaligned { .. } | Self::Ragged { .. } | Self::ElementWidth { .. } => {
                PgSqlErrorCode::ERRCODE_INTERNAL_ERROR
            }
            Self::Short { .. }
            | Self::Magic
            | Self::Format { .. }
            | Self::Version { .. }
            | Self::Entry { .. }
            | Self::MissingArray(_) => PgSqlErrorCode::ERRCODE_DATA_CORRUPTED,
        }
    }
}
