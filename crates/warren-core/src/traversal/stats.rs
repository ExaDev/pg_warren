//! What a traversal did: nodes visited, edges traversed, whether a limit cut
//! it short (DESIGN.md §9.3, `warren.last_stats()`).

use super::{Interrupts, TraversalError};

/// How often, in units of work (nodes visited plus edges traversed), a
/// traversal polls for interrupts.
const POLL_INTERVAL: u32 = 1 << 16;

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

/// Running stats plus the interrupt poll, threaded through a traversal.
pub(crate) struct Progress<'c> {
    stats: TraversalStats,
    interrupts: &'c mut dyn Interrupts,
    until_poll: u32,
}

impl<'c> Progress<'c> {
    pub(crate) fn new(interrupts: &'c mut dyn Interrupts) -> Self {
        Self { stats: TraversalStats::default(), interrupts, until_poll: 0 }
    }

    /// Counts a visited node, a unit of work.
    pub(crate) fn node_visited(&mut self) -> Result<(), TraversalError> {
        self.stats.nodes_visited += 1;
        self.work()
    }

    /// Counts a scanned row entry, a unit of work, so edge scans that admit
    /// no node (late BFS levels) are still polled.
    pub(crate) fn edge_traversed(&mut self) -> Result<(), TraversalError> {
        self.stats.edges_traversed += 1;
        self.work()
    }

    /// Polls for interrupts on the first unit of work and then every
    /// [`POLL_INTERVAL`]th.
    fn work(&mut self) -> Result<(), TraversalError> {
        if self.until_poll == 0 {
            self.interrupts.check()?;
            self.until_poll = POLL_INTERVAL;
        }
        self.until_poll -= 1;
        Ok(())
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
    use crate::traversal::{Cancellation, TraversalError, Uninterrupted};

    #[test]
    fn progress_counts_nodes_and_edges() {
        let mut uninterrupted = Uninterrupted;
        let mut progress = Progress::new(&mut uninterrupted);
        progress.node_visited().unwrap();
        progress.edge_traversed().unwrap();
        progress.edge_traversed().unwrap();
        progress.truncate();
        assert_eq!(
            progress.finish(),
            TraversalStats { nodes_visited: 1, edges_traversed: 2, truncated: true }
        );
    }

    #[test]
    fn progress_polls_on_the_first_unit_of_work_and_every_64k_after() {
        let cancellation = Cancellation::new();
        let mut polled = cancellation.clone();
        let mut progress = Progress::new(&mut polled);
        progress.node_visited().unwrap();
        cancellation.cancel();
        for unit in 1..POLL_INTERVAL {
            if unit % 2 == 0 {
                progress.node_visited().unwrap();
            } else {
                progress.edge_traversed().unwrap();
            }
        }
        assert_eq!(progress.edge_traversed(), Err(TraversalError::Cancelled));

        let mut polled = cancellation.clone();
        let mut cancelled = Progress::new(&mut polled);
        assert_eq!(cancelled.edge_traversed(), Err(TraversalError::Cancelled));
    }
}
