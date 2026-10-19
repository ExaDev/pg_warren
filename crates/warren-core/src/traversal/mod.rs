//! Traversal over a graph's CSR: BFS frontiers, reachability, shortest paths
//! and components (DESIGN.md §9), in dense ids. Mapping external ids to and
//! from dense ids is the adapter's job.
//!
//! A traversal starts from an [`Adjacency`]: the CSRs of the selected
//! relations, read in one [`Direction`]. It reports [`TraversalStats`]
//! alongside its result, and stops with [`TraversalError::Cancelled`] when
//! its [`Interrupts`] poll says so.

mod adjacency;
mod bfs;
mod components;
mod dijkstra;
mod error;
mod interrupts;
mod level;
mod limits;
mod node_set;
mod reachable;
mod scratch;
mod shortest_path;
mod stats;
mod walk;

#[cfg(test)]
mod example;
#[cfg(test)]
mod reference;

pub use adjacency::{Adjacency, Direction, DistinctNeighbours, RelationSelection};
pub use bfs::{bfs, bfs_each, Visit};
pub use components::components;
pub use dijkstra::{dijkstra, WeightedPath};
pub use error::TraversalError;
pub use interrupts::{Cancellation, Cancelled, Interrupts, Uninterrupted};
pub use limits::Limits;
pub use node_set::NodeSet;
pub use reachable::{reachable, reachable_set, reachable_set_each};
pub use shortest_path::shortest_path;
pub use stats::{TraversalStats, Traversed};
pub use walk::{HopRange, Step};
