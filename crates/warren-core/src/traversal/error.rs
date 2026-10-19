//! The traversal feature's error type.

use thiserror::Error;

use crate::graph::{DenseId, GraphError};
use crate::interrupts::Cancelled;

/// Why a traversal could not run to completion.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum TraversalError {
    /// Direction `in` or `both` reads the reverse CSR, which this graph was
    /// built without (`build_reverse = false`).
    #[error("direction 'in' or 'both' needs the reverse CSR, and this graph has none")]
    ReverseCsrMissing,
    /// A selected relation is not in the graph's relation dictionary.
    #[error("relation {relation} is not in the graph")]
    UnknownRelation {
        /// The relation index.
        relation: usize,
    },
    /// A start or target node is not a dense id of the id space.
    #[error("node {node} lies outside the id space")]
    NodeOutsideIdSpace {
        /// The offending dense id.
        node: DenseId,
    },
    /// A node filter must cover the id space of the graph it filters.
    #[error("node filter covers {found} ids, expected {expected}")]
    NodeFilterIdSpace {
        /// The graph's id space.
        expected: u64,
        /// The filter's id space.
        found: u64,
    },
    /// Shortest weighted paths need weights that are not negative or NaN.
    #[error("edge {from} → {to} has weight {weight}; weights must not be negative or NaN")]
    InvalidWeight {
        /// The edge's source, in the direction traversed.
        from: DenseId,
        /// The edge's target, in the direction traversed.
        to: DenseId,
        /// The offending weight.
        weight: f64,
    },
    /// The traversal's [`Interrupts`](crate::interrupts::Interrupts) poll asked it to stop.
    #[error("traversal cancelled")]
    Cancelled,
    /// A graph-sized scratch buffer could not be allocated.
    #[error(transparent)]
    Graph(#[from] GraphError),
}

impl From<Cancelled> for TraversalError {
    fn from(_: Cancelled) -> Self {
        Self::Cancelled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancelled_poll_is_a_cancelled_traversal() {
        assert_eq!(TraversalError::from(Cancelled), TraversalError::Cancelled);
    }
}
