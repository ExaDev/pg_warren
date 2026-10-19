//! Traversal over a graph's CSR: BFS frontiers, reachability, shortest paths
//! and components (DESIGN.md §9), in dense ids. Mapping external ids to and
//! from dense ids is the adapter's job.
//!
//! A traversal starts from an [`Adjacency`]: the CSRs of the selected
//! relations, read in one [`Direction`]. It reports [`TraversalStats`]
//! alongside its result, and stops with [`TraversalError::Cancelled`] once
//! its [`Cancellation`] is raised.

mod adjacency;
mod bfs;
mod cancellation;
mod error;
mod level;
mod limits;
mod node_set;
mod reachable;
mod scratch;
mod stats;

#[cfg(test)]
mod example;
#[cfg(test)]
mod reference;

pub use adjacency::{Adjacency, Direction, RelationSelection};
pub use bfs::{bfs, Visit};
pub use cancellation::Cancellation;
pub use error::TraversalError;
pub use limits::Limits;
pub use node_set::NodeSet;
pub use reachable::{reachable, reachable_set};
pub use stats::{TraversalStats, Traversed};
