//! The caller's bounds on a traversal (DESIGN.md §9.3).

/// Bounds on a traversal. `None` is unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Limits {
    /// The deepest level to reach; starts are at depth 0. Reaching it is
    /// the requested result, not truncation.
    pub max_depth: Option<u32>,
    /// The most rows to return. Stopping because of it sets
    /// [`TraversalStats::truncated`](super::TraversalStats::truncated).
    pub max_results: Option<u64>,
}
