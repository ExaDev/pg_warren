//! The compute feature's error type.

use thiserror::Error;

use super::Dtype;
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
    /// Operands of one kernel must share an element type.
    #[error("element type mismatch: expected {expected}, found {found}")]
    DtypeMismatch {
        /// The element type of the output.
        expected: Dtype,
        /// The offending operand's element type.
        found: Dtype,
    },
    /// Vectors must cover the id space of the graph they are used with.
    #[error("vector covers {found} dense ids, expected the id space {expected}")]
    IdSpaceMismatch {
        /// The graph's (or the output's) id space.
        expected: u64,
        /// The offending vector's id space.
        found: u64,
    },
    /// Vectors combined row by row must have compatible widths.
    #[error("vector width {found} does not match width {expected}")]
    WidthMismatch {
        /// The width required.
        expected: usize,
        /// The offending vector's width.
        found: usize,
    },
    /// A selected relation is not in the graph's relation dictionary.
    #[error("relation {relation} is not in the graph")]
    UnknownRelation {
        /// The relation index.
        relation: usize,
    },
    /// A semiring name is not one of the v1 semirings.
    #[error("unknown semiring \"{0}\"")]
    UnknownSemiring(String),
    /// A binary operator name is not a GraphBLAS binary operator.
    #[error("unknown binary operator \"{0}\"")]
    UnknownBinaryOp(String),
    /// A reduction name is not one of `sum|max|min|l1|l2|nnz`.
    #[error("unknown reduction \"{0}\": expected sum, max, min, l1, l2 or nnz")]
    UnknownReduction(String),
    /// A norm name is not one of `l1|l2|linf`.
    #[error("unknown norm \"{0}\": expected l1, l2 or linf")]
    UnknownNorm(String),
    /// Arithmetic failed as the same SQL arithmetic would.
    #[error(transparent)]
    Arithmetic(#[from] ArithmeticError),
    /// The caller's interrupts asked the kernel to stop.
    #[error("compute cancelled")]
    Cancelled,
}

impl From<Cancelled> for ComputeError {
    fn from(_: Cancelled) -> Self {
        Self::Cancelled
    }
}

/// An arithmetic error, with the message Postgres raises for it, so the
/// compiled path and SQL agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ArithmeticError {
    /// An `i64` result or cast out of range (SQLSTATE 22003).
    #[error("bigint out of range")]
    BigintOutOfRange,
    /// A finite float result that overflows (SQLSTATE 22003).
    #[error("value out of range: overflow")]
    FloatOverflow,
    /// A non-zero float result that underflows to zero (SQLSTATE 22003).
    #[error("value out of range: underflow")]
    FloatUnderflow,
}
