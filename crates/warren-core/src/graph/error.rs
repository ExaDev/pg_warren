//! The graph feature's error type.

use thiserror::Error;

use super::DenseId;

/// Why a graph structure could not be built or loaded.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GraphError {
    /// A graph-sized buffer could not be allocated.
    #[error("out of memory allocating a buffer of {elements} elements")]
    OutOfMemory {
        /// The number of elements requested.
        elements: usize,
    },
    /// A relation dictionary can hold at most 2³² relations.
    #[error("too many relations: a graph has at most 2^32")]
    TooManyRelations,
    /// An id space of 2⁶⁴ − 1 leaves no room for its CSR offsets.
    #[error("id space too large")]
    IdSpaceTooLarge,
    /// A CSR needs `id_space + 1` offsets.
    #[error("CSR has {found} offsets, expected {expected}")]
    OffsetsLength {
        /// `id_space + 1`.
        expected: u64,
        /// The number of offsets given.
        found: u64,
    },
    /// A CSR's first offset must be 0.
    #[error("CSR offsets do not start at 0")]
    FirstOffsetNotZero,
    /// A CSR's offsets must never decrease.
    #[error("CSR offsets decrease after row {row}")]
    OffsetsNotMonotone {
        /// The row whose end offset is below its start offset.
        row: DenseId,
    },
    /// A CSR's last offset must equal its number of targets.
    #[error("CSR final offset is {found}, expected {expected}")]
    FinalOffset {
        /// The number of targets.
        expected: u64,
        /// The last offset.
        found: u64,
    },
    /// Every CSR target must be a dense id inside the id space.
    #[error("CSR row {row} has target {target} outside the id space")]
    TargetOutsideIdSpace {
        /// The row holding the target.
        row: DenseId,
        /// The offending target.
        target: DenseId,
    },
    /// Every CSR row must be strictly ascending.
    #[error("CSR row {row} is not strictly ascending")]
    RowNotSorted {
        /// The offending row.
        row: DenseId,
    },
    /// A weighted CSR needs one weight per target.
    #[error("CSR has {found} weights, expected {expected}")]
    WeightsLength {
        /// The number of targets.
        expected: u64,
        /// The number of weights given.
        found: u64,
    },
    /// A CSR's target width must represent every dense id of its id space.
    #[error("id space exceeds the CSR target width")]
    IdSpaceExceedsTargetWidth,
    /// An edge endpoint is not a dense id of the id space.
    #[error("edge {from} → {to} lies outside the id space")]
    EdgeOutsideIdSpace {
        /// The edge's source (named `from`: thiserror reserves `source`).
        from: DenseId,
        /// The edge's target.
        to: DenseId,
    },
    /// An edge's relation is not in the relation dictionary.
    #[error("relation {relation} is not in the relation dictionary")]
    RelationOutsideDictionary {
        /// The relation index.
        relation: u64,
    },
}
