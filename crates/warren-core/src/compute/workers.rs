//! The compute threads: a dedicated `rayon` pool sized by the
//! `warren.threads` GUC, and how a kernel running on it is interrupted
//! (DESIGN.md §6.9).
//!
//! Only the calling (main) thread may call into Postgres, so a kernel runs
//! as a job on the pool while the caller waits on the main thread, polling
//! its [`Interrupts`] hook. When the poll says stop, or unwinds (a Postgres
//! ERROR raised by `check_for_interrupts!`), the caller raises the job's
//! [`Cancellation`] flag, which worker loops check at least every 64K rows.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use rayon::{ThreadPool, ThreadPoolBuilder};

use super::ComputeError;
use crate::interrupts::{Cancellation, Interrupts};

/// How often the caller polls its interrupts while a job runs.
const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// A pool of compute threads.
#[derive(Debug)]
pub struct Workers {
    pool: ThreadPool,
}

impl Workers {
    /// A pool of `threads` compute threads.
    pub fn new(threads: usize) -> Result<Self, ComputeError> {
        if threads == 0 {
            return Err(ComputeError::NoThreads);
        }
        let pool = ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|index| format!("warren-compute-{index}"))
            .build()
            .map_err(|error| ComputeError::Threads(error.to_string()))?;
        Ok(Self { pool })
    }

    /// The number of compute threads.
    pub fn threads(&self) -> usize {
        self.pool.current_num_threads()
    }

    /// Runs `job` on the pool. The caller's thread polls `interrupts` until
    /// the job finishes; once a poll asks to stop, the job's cancellation
    /// flag is raised and the result is [`ComputeError::Cancelled`].
    pub fn run<R: Send>(
        &self,
        interrupts: &mut impl Interrupts,
        job: impl FnOnce(&Cancellation) -> Result<R, ComputeError> + Send,
    ) -> Result<R, ComputeError> {
        interrupts.check()?;
        let cancellation = Cancellation::new();
        let finished = AtomicBool::new(false);
        let caller = thread::current();
        let mut result = Err(ComputeError::Cancelled);
        let mut interrupted = false;
        self.pool.in_place_scope(|scope| {
            let raise_on_unwind = RaiseOnDrop(&cancellation);
            scope.spawn(|_| {
                result = job(&cancellation);
                finished.store(true, Ordering::Release);
                caller.unpark();
            });
            while !finished.load(Ordering::Acquire) {
                if interrupts.check().is_err() {
                    interrupted = true;
                    break;
                }
                thread::park_timeout(POLL_INTERVAL);
            }
            // Stops an interrupted job (and is harmless after a finished
            // one); an unwinding poll drops the guard too. Either way the
            // scope then waits for the job before returning.
            drop(raise_on_unwind);
        });
        match interrupted {
            true => Err(ComputeError::Cancelled),
            false => result,
        }
    }
}

/// Raises a cancellation when dropped, including while unwinding.
struct RaiseOnDrop<'a>(&'a Cancellation);

impl Drop for RaiseOnDrop<'_> {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

// Not under Miri: the compute pool's work stealing runs through
// crossbeam-epoch, which Miri's Stacked Borrows model rejects (a known
// report against crossbeam-epoch 0.9, not this crate, which has no unsafe).
#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;
    use crate::interrupts::{Cancelled, Uninterrupted};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::time::Instant;

    /// Waits until `cancellation` is raised, giving up after ten seconds.
    fn wait_for(cancellation: &Cancellation) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if cancellation.is_cancelled() {
                return true;
            }
            thread::yield_now();
        }
        false
    }

    /// Asks to stop on the `n`th poll.
    struct StopAfter(usize);

    impl Interrupts for StopAfter {
        fn check(&mut self) -> Result<(), Cancelled> {
            self.0 = self.0.saturating_sub(1);
            if self.0 == 0 {
                return Err(Cancelled);
            }
            Ok(())
        }
    }

    #[test]
    fn a_job_runs_on_the_pool_and_returns_its_result() {
        let workers = Workers::new(3).unwrap();
        assert_eq!(workers.threads(), 3);
        let result = workers.run(&mut Uninterrupted, |_| {
            Ok((rayon::current_thread_index().is_some(), rayon::current_num_threads()))
        });
        assert_eq!(result, Ok((true, 3)));
    }

    #[test]
    fn a_pool_needs_a_thread() {
        assert_eq!(Workers::new(0).unwrap_err(), ComputeError::NoThreads);
    }

    #[test]
    fn an_interrupt_before_the_job_starts_skips_it() {
        let workers = Workers::new(1).unwrap();
        let mut ran = false;
        let result = workers.run(&mut StopAfter(1), |_| {
            ran = true;
            Ok(())
        });
        assert_eq!(result, Err(ComputeError::Cancelled));
        assert!(!ran);
    }

    #[test]
    fn an_interrupt_while_the_job_runs_raises_its_cancellation() {
        let workers = Workers::new(2).unwrap();
        let result = workers.run(&mut StopAfter(3), |cancellation| {
            assert!(wait_for(cancellation));
            Ok(())
        });
        assert_eq!(result, Err(ComputeError::Cancelled));
    }

    /// Unwinds like a Postgres ERROR raised by `check_for_interrupts!`.
    struct Error;

    impl Interrupts for Error {
        fn check(&mut self) -> Result<(), Cancelled> {
            panic!("canceling statement due to user request");
        }
    }

    #[test]
    fn an_unwinding_poll_raises_the_cancellation_and_waits_for_the_job() {
        let workers = Workers::new(2).unwrap();
        let saw_cancellation = AtomicBool::new(false);
        let unwound = catch_unwind(AssertUnwindSafe(|| {
            workers.run(&mut StopAfterThenError(2), |cancellation| {
                saw_cancellation.store(wait_for(cancellation), Ordering::Relaxed);
                Ok(())
            })
        }));
        assert!(unwound.is_err());
        assert!(saw_cancellation.load(Ordering::Relaxed));
    }

    /// Passes the first `n - 1` polls, then unwinds.
    struct StopAfterThenError(usize);

    impl Interrupts for StopAfterThenError {
        fn check(&mut self) -> Result<(), Cancelled> {
            self.0 = self.0.saturating_sub(1);
            if self.0 == 0 {
                return Error.check();
            }
            Ok(())
        }
    }
}
