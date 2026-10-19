//! Cooperative cancellation: a flag the caller raises and a traversal polls
//! every ~64K visited nodes (DESIGN.md §6.9, §9.3).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A cancellation flag shared between the caller and a running traversal.
/// Clones share the flag: the adapter keeps one and raises it (for example
/// on a Postgres interrupt), the traversal polls the other.
#[derive(Debug, Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    /// A flag that is not raised.
    pub fn new() -> Self {
        Self::default()
    }

    /// Raises the flag: traversals polling it stop with
    /// [`TraversalError::Cancelled`](super::TraversalError::Cancelled).
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether the flag has been raised.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_one_flag() {
        let cancellation = Cancellation::new();
        let watcher = cancellation.clone();
        assert!(!watcher.is_cancelled());
        cancellation.cancel();
        assert!(watcher.is_cancelled());
    }
}
