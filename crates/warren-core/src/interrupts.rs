//! Cooperative interruption of long-running domain work (traversals and
//! compute kernels): a poll hook called every ~64K units of work, and a
//! flag shared across threads (DESIGN.md §6.9, §9.3).
//!
//! On the Postgres main thread the adapter implements [`Interrupts`] with
//! `pgrx::check_for_interrupts!()`, which raises the ERROR and unwinds
//! through the domain code (every buffer is owned, and no FFI boundary is
//! crossed). On worker threads, which may not call Postgres, the main
//! thread raises a shared [`Cancellation`] flag instead.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use thiserror::Error;

/// The poll long-running work makes every ~64K units of work.
pub trait Interrupts {
    /// Returns [`Cancelled`] if the work should stop now.
    fn check(&mut self) -> Result<(), Cancelled>;
}

/// The caller asked the work to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("cancelled")]
pub struct Cancelled;

/// Never interrupts: for callers with nothing to poll.
#[derive(Debug, Clone, Copy, Default)]
pub struct Uninterrupted;

impl Interrupts for Uninterrupted {
    fn check(&mut self) -> Result<(), Cancelled> {
        Ok(())
    }
}

/// A cancellation flag shared between threads: clones share the flag, so
/// one thread keeps a clone and raises it while work on another polls its
/// own clone.
#[derive(Debug, Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    /// A flag that is not raised.
    pub fn new() -> Self {
        Self::default()
    }

    /// Raises the flag: work polling it stops with [`Cancelled`].
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether the flag has been raised.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

impl Interrupts for Cancellation {
    fn check(&mut self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            return Err(Cancelled);
        }
        Ok(())
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

    #[test]
    fn a_cancellation_is_an_interrupt_poll() {
        let mut cancellation = Cancellation::new();
        assert_eq!(cancellation.check(), Ok(()));
        cancellation.cancel();
        assert_eq!(cancellation.check(), Err(Cancelled));
    }

    #[test]
    fn uninterrupted_never_interrupts() {
        assert_eq!(Uninterrupted.check(), Ok(()));
    }
}
