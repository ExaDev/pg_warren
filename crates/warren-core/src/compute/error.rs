//! The compute feature's error type.

use thiserror::Error;

use crate::traversal::Cancelled;

/// Why a compute kernel could not run to completion.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ComputeError {
    /// A buffer would take the compute memory in use past its limit.
    #[error(
        "compute memory limit exceeded: {requested} bytes requested, {in_use} of {limit} in use"
    )]
    MemoryLimit {
        /// The bytes requested.
        requested: u64,
        /// The bytes already charged.
        in_use: u64,
        /// The budget's limit in bytes.
        limit: u64,
    },
    /// The allocator could not provide a buffer, or its size overflows.
    #[error("out of memory allocating {bytes} bytes")]
    OutOfMemory {
        /// The bytes requested (saturated at 2⁶⁴ − 1).
        bytes: u64,
    },
    /// A dtype name is not one of `f32`, `f64`, `i64`, `bool`.
    #[error("unknown dtype \"{0}\": expected f32, f64, i64 or bool")]
    UnknownDtype(String),
    /// A vector needs at least one element per row.
    #[error("a vector's width must be at least 1")]
    ZeroWidth,
    /// A vector's values must fill `id_space × width` elements.
    #[error("vector has {found} values, expected {expected}")]
    VectorLength {
        /// `id_space × width`.
        expected: u64,
        /// The number of values given.
        found: u64,
    },
    /// A pool of compute threads needs at least one thread.
    #[error("compute needs at least one thread")]
    NoThreads,
    /// The compute threads could not be started.
    #[error("could not start the compute threads: {0}")]
    Threads(String),
    /// The caller's interrupts asked the kernel to stop.
    #[error("compute cancelled")]
    Cancelled,
}

impl From<Cancelled> for ComputeError {
    fn from(_: Cancelled) -> Self {
        Self::Cancelled
    }
}
