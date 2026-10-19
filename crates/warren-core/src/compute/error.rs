//! The compute feature's error type.

use thiserror::Error;

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
}
