//! The traversal feature's error type.

use thiserror::Error;

use crate::graph::{DenseId, GraphError};

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
    /// A walk step's hop range must have `min <= max <= HopRange::CAP`.
    #[error("hop range {min}..{max} is invalid: need min <= max <= {cap}", cap = super::HopRange::CAP)]
    InvalidHopRange {
        /// The fewest hops asked for.
        min: u32,
        /// The most hops asked for.
        max: u32,
    },
    /// A walk has more walks than a `u64` can count.
    #[error("the walk has more than 2^64 - 1 walks")]
    WalkCountOverflow,
    /// The traversal's [`Interrupts`](super::Interrupts) poll asked it to stop.
    #[error("traversal cancelled")]
    Cancelled,
    /// A graph-sized scratch buffer could not be allocated.
    #[error(transparent)]
    Graph(#[from] GraphError),
}
