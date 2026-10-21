//! The community feature's error type.

use thiserror::Error;

use crate::compute::{ComputeError, Dtype};
use crate::graph::{DenseId, GraphError};

/// Why a community operation could not run to completion.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum CommunityError {
    /// A graph structure could not be built or read.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// A vector could not be allocated, or the work was cancelled.
    #[error(transparent)]
    Compute(#[from] ComputeError),
    /// A label vector has one label per node.
    #[error("a label vector must have width 1, not {0}")]
    LabelWidth(usize),
    /// Labels are numbers.
    #[error("a label vector must be numeric (i64, f64 or f32), not {0}")]
    LabelDtype(Dtype),
    /// Labels are integers, so that they can be the quotient's node ids.
    #[error("the label of dense id {node} is {value}, which is not an integer")]
    NotIntegral {
        /// The node.
        node: DenseId,
        /// Its label.
        value: f64,
    },
    /// A label vector covers the id space of its version.
    #[error("the label vector covers {found} dense ids, expected the id space {expected}")]
    LabelLength {
        /// The version's id space.
        expected: u64,
        /// The vector's.
        found: u64,
    },
    /// Every label is a node of the quotient.
    #[error("the label {label} of dense id {node} is not a node of the quotient")]
    UnknownLabel {
        /// The node.
        node: DenseId,
        /// Its label.
        label: i64,
    },
    /// `project` aggregates with `sum`, `mean`, `max`, `min` or `count`.
    #[error("unknown aggregate \"{0}\": expected sum, mean, max, min or count")]
    UnknownAggregate(String),
}
