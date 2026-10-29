//! Communities (DESIGN.md §6.6, §6.7): two-level computation over a
//! partition of a graph's nodes. A label vector names each node's
//! community; contraction builds the quotient graph, one node per
//! community with the edges between communities aggregated (`PᵀAP`);
//! projection aggregates a vector over each community's members (`Pᵀx`)
//! and lifting broadcasts a community's value back to them (`P·y`).
//! Modularity, Louvain's local moving and Leiden's refinement work on the
//! undirected weighted view of a graph, which contraction preserves.

mod contraction;
mod error;
mod labels;
mod leiden;
mod local_moving;
mod modularity;
mod projection;
mod random;
mod refinement;
mod undirected;

pub use contraction::{contract, Contraction};
pub use error::CommunityError;
pub use labels::Labels;
pub use leiden::{leiden, Found, Leiden, Multilevel, MOST_ITERATIONS};
pub use local_moving::{local_moving, LocalMoving, Moved};
pub use modularity::modularity;
pub use projection::{lift, project, Aggregate};
pub use refinement::{refine, Refined, Refinement};
pub use undirected::{Undirected, Weighting};
