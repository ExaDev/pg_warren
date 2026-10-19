//! The graph feature's error type.

use thiserror::Error;

/// Why a graph structure could not be built or loaded.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GraphError {
    /// A graph-sized buffer could not be allocated.
    #[error("out of memory allocating a buffer of {elements} elements")]
    OutOfMemory {
        /// The number of elements requested.
        elements: usize,
    },
}
