//! `gather`: fan per-node messages out over the graph's edges and combine
//! them at each receiver, in-engine (DESIGN.md §6.4, tier 2).

mod combiner;

pub use combiner::Combiner;
