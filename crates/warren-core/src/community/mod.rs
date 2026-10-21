//! Communities (DESIGN.md §6.6, §6.7): two-level computation over a
//! partition of a graph's nodes. A label vector names each node's
//! community; contraction builds the quotient graph, one node per
//! community with the edges between communities aggregated (`PᵀAP`);
//! projection aggregates a vector over each community's members (`Pᵀx`)
//! and lifting broadcasts a community's value back to them (`P·y`).

mod contraction;
mod error;
mod labels;
mod projection;

pub use contraction::{contract, Contraction};
pub use error::CommunityError;
pub use labels::Labels;
pub use projection::{lift, project, Aggregate};
