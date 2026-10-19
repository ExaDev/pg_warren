//! What a traversal did: nodes visited, edges traversed, whether a limit cut
//! it short (DESIGN.md §9.3, `warren.last_stats()`).

use super::{Cancellation, TraversalError};

/// How often, in visited nodes, a traversal polls its cancellation flag.
const CANCELLATION_INTERVAL: u32 = 1 << 16;

/// The work a traversal did, recorded for `warren.last_stats()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TraversalStats {
    /// Nodes admitted to a frontier (or settled), starts included.
    pub nodes_visited: u64,
    /// CSR row entries scanned.
    pub edges_traversed: u64,
    /// Whether the scan stopped early because of `max_results`. Reaching
    /// the caller's own `max_depth` is the requested result, not
    /// truncation.
    pub truncated: bool,
}

/// A traversal's result with the stats of the work that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct Traversed<T> {
    /// The result.
    pub result: T,
    /// The work done.
    pub stats: TraversalStats,
}

/// Running stats plus the cancellation poll, threaded through a traversal.
pub(crate) struct Progress<'c> {
    stats: TraversalStats,
    cancellation: &'c Cancellation,
    until_poll: u32,
}

impl<'c> Progress<'c> {
    pub(crate) fn new(cancellation: &'c Cancellation) -> Self {
        Self { stats: TraversalStats::default(), cancellation, until_poll: 0 }
    }

    /// Counts a visited node, polling the cancellation flag on the first
    /// and then every [`CANCELLATION_INTERVAL`]th.
    pub(crate) fn node_visited(&mut self) -> Result<(), TraversalError> {
        self.stats.nodes_visited += 1;
        if self.until_poll == 0 {
            if self.cancellation.is_cancelled() {
                return Err(TraversalError::Cancelled);
            }
            self.until_poll = CANCELLATION_INTERVAL;
        }
        self.until_poll -= 1;
        Ok(())
    }

    pub(crate) fn edge_traversed(&mut self) {
        self.stats.edges_traversed += 1;
    }

    /// Records that a limit stopped the scan early.
    pub(crate) fn truncate(&mut self) {
        self.stats.truncated = true;
    }

    pub(crate) fn finish(self) -> TraversalStats {
        self.stats
    }

    /// `result` with the stats so far.
    pub(crate) fn traversed<T>(self, result: T) -> Traversed<T> {
        Traversed { result, stats: self.finish() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traversal::{Cancellation, TraversalError};

    #[test]
    fn progress_counts_nodes_and_edges() {
        let cancellation = Cancellation::new();
        let mut progress = Progress::new(&cancellation);
        progress.node_visited().unwrap();
        progress.edge_traversed();
        progress.edge_traversed();
        progress.truncate();
        assert_eq!(
            progress.finish(),
            TraversalStats { nodes_visited: 1, edges_traversed: 2, truncated: true }
        );
    }

    #[test]
    fn progress_polls_the_cancellation_on_the_first_node_and_every_64k_after() {
        let cancellation = Cancellation::new();
        let mut progress = Progress::new(&cancellation);
        progress.node_visited().unwrap();
        cancellation.cancel();
        for _ in 1..CANCELLATION_INTERVAL {
            progress.node_visited().unwrap();
        }
        assert_eq!(progress.node_visited(), Err(TraversalError::Cancelled));

        let mut cancelled = Progress::new(&cancellation);
        assert_eq!(cancelled.node_visited(), Err(TraversalError::Cancelled));
    }
}
