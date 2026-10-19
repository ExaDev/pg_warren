//! The compute feature's error type.

use thiserror::Error;

use super::{Combiner, Dtype, MessageKind};
use crate::interrupts::Cancelled;

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
    /// A vector's stored bytes do not encode `id_space × width` elements
    /// of its dtype.
    #[error("vector has {found} bytes, expected {expected} bytes of valid elements")]
    VectorBytes {
        /// The bytes the shape needs.
        expected: u64,
        /// The bytes found.
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
    /// A combiner name is not one of the `gather` combiners.
    #[error(
        "unknown combiner \"{0}\": expected collect, count, sum, min, max, first, concat, \
         bool_or or bool_and"
    )]
    UnknownCombiner(String),
    /// A combiner does not accept the messages' kind.
    #[error("combine := {combiner} does not accept {message} messages")]
    CombinerType {
        /// The combiner.
        combiner: Combiner,
        /// The messages' kind.
        message: MessageKind,
    },
    /// A message's sender is not a dense id of the graph.
    #[error("message sender {sender} is outside the id space {id_space}")]
    SenderOutsideIdSpace {
        /// The sender's dense id.
        sender: u64,
        /// The graph's id space.
        id_space: u64,
    },
    /// An expression refers to an operand vector that was not given.
    #[error("expression refers to operand {index}, but {operands} were given")]
    UnknownOperand {
        /// The operand index the expression refers to.
        index: usize,
        /// The number of operands given.
        operands: usize,
    },
    /// An expression reads an operand vector as the wrong element type.
    #[error("expression reads operand {index} as {expected}, but it is {found}")]
    OperandType {
        /// The operand index.
        index: usize,
        /// The type the expression reads.
        expected: Dtype,
        /// The operand's element type.
        found: Dtype,
    },
    /// An expression is NULL for an element of a vector type that has no
    /// NULL representation (SQLSTATE 22004).
    #[error("null value cannot be stored in a {dtype} vector")]
    NullResult {
        /// The vector's element type.
        dtype: Dtype,
    },
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
    /// Division or modulo by zero (SQLSTATE 22012).
    #[error("division by zero")]
    DivisionByZero,
    /// `sqrt` of a negative number (SQLSTATE 2201F).
    #[error("cannot take square root of a negative number")]
    SqrtOfNegative,
    /// `ln` or `log` of zero (SQLSTATE 2201E).
    #[error("cannot take logarithm of zero")]
    LogOfZero,
    /// `ln` or `log` of a negative number (SQLSTATE 2201E).
    #[error("cannot take logarithm of a negative number")]
    LogOfNegative,
    /// `power(0, b)` with `b < 0` (SQLSTATE 2201F).
    #[error("zero raised to a negative power is undefined")]
    ZeroToNegativePower,
    /// `power(a, b)` with `a < 0` and `b` not an integer (SQLSTATE 2201F).
    #[error("a negative number raised to a non-integer power yields a complex result")]
    ComplexPower,
}
