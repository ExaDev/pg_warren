//! The compute pool (DESIGN.md §6.9): `warren.threads`, its size (default
//! half the cores, at least 1). `warren.max_compute_memory`, the compute
//! memory a call may hold, is defined with the cache's bound, which it
//! shares (`cache::compute_memory`).
//!
//! The pool is built once per backend and rebuilt when `warren.threads`
//! changes. Its threads block every signal: Postgres's handlers must run
//! on the main thread, which is the only one that may touch backend state.

use std::cell::RefCell;
use std::rc::Rc;
use std::thread::available_parallelism;

use pgrx::{GucContext, GucFlags, GucRegistry, GucSetting};
use warren_core::compute::{ComputeError, Workers};

/// The most compute threads a backend may run.
const MAX_THREADS: i32 = 1024;

/// `warren.threads`; its boot value is set from the core count at load.
static THREADS: GucSetting<i32> = GucSetting::<i32>::new(1);

/// Half the cores, at least one.
fn default_threads() -> i32 {
    let cores = available_parallelism().map_or(1, usize::from);
    i32::try_from(cores / 2).unwrap_or(MAX_THREADS).clamp(1, MAX_THREADS)
}

/// Registers `warren.threads`. Called once, from `_PG_init`.
pub(crate) fn define_compute_settings() {
    // SAFETY: `_PG_init` runs on the main thread before the GUC is
    // registered, so nothing else reads or writes the setting; its value
    // at registration becomes the boot value.
    unsafe { *THREADS.as_ptr() = default_threads() };
    GucRegistry::define_int_guc(
        c"warren.threads",
        c"Threads a compute kernel (spmv, spmm, vector_map, reductions, gather, triangles, \
           eigen, distance) runs on.",
        c"Defaults to half the cores. Compute threads never call into Postgres.",
        &THREADS,
        1,
        MAX_THREADS,
        GucContext::Userset,
        GucFlags::default(),
    );
}

thread_local! {
    /// The backend's compute pool, if built.
    static WORKERS: RefCell<Option<Rc<Workers>>> = const { RefCell::new(None) };
}

/// The compute pool of `warren.threads` threads, built (or rebuilt) on
/// first use after a change.
pub(crate) fn workers() -> Result<Rc<Workers>, ComputeError> {
    let threads = usize::try_from(THREADS.get()).unwrap_or(1);
    if let Some(workers) = WORKERS.with_borrow(|w| w.clone().filter(|w| w.threads() == threads)) {
        return Ok(workers);
    }
    let workers = Rc::new(with_signals_blocked(|| Workers::new(threads))?);
    WORKERS.set(Some(Rc::clone(&workers)));
    Ok(workers)
}

/// Runs `f` with every signal blocked on this thread, so threads it starts
/// inherit the blocked mask, then restores the mask.
fn with_signals_blocked<R>(f: impl FnOnce() -> R) -> R {
    // SAFETY: sigset_t is plain data, initialised by sigfillset and
    // pthread_sigmask before it is read.
    let (mut all, mut previous) = unsafe { (std::mem::zeroed(), std::mem::zeroed()) };
    // SAFETY: both pointers are to live locals; these calls only fail for an
    // invalid `how`, which SIG_BLOCK and SIG_SETMASK are not.
    unsafe {
        libc::sigfillset(&mut all);
        libc::pthread_sigmask(libc::SIG_BLOCK, &all, &mut previous);
    }
    let result = f();
    // SAFETY: restores the mask read above.
    unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()) };
    result
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;
    use warren_core::interrupts::Uninterrupted;

    use super::{default_threads, workers};

    #[pg_test]
    fn threads_default_to_half_the_cores_and_memory_to_4gb() -> Result<(), Error> {
        let threads = Spi::get_one::<String>("SELECT current_setting('warren.threads')")?;
        assert_eq!(threads, Some(default_threads().to_string()));
        let memory = Spi::get_one::<String>("SELECT current_setting('warren.max_compute_memory')")?;
        assert_eq!(memory.as_deref(), Some("4GB"));
        Ok(())
    }

    #[pg_test]
    fn the_pool_follows_warren_threads_and_blocks_signals() -> Result<(), Error> {
        Spi::run("SET LOCAL warren.threads = 3")?;
        let pool = workers().map_err(|_| Error::InvalidPosition)?;
        assert_eq!(pool.threads(), 3);
        let blocked = pool
            .run(&mut Uninterrupted, |_| {
                // SAFETY: reads this thread's signal mask into a local.
                let mut mask = unsafe { std::mem::zeroed() };
                // SAFETY: a NULL new mask only queries; both pointers are valid.
                unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut mask) };
                // SAFETY: `mask` was filled in above.
                let blocked = |signal| unsafe { libc::sigismember(&mask, signal) == 1 };
                Ok([libc::SIGINT, libc::SIGTERM, libc::SIGALRM, libc::SIGUSR1].map(blocked))
            })
            .map_err(|_| Error::InvalidPosition)?;
        assert_eq!(blocked, [true; 4]);
        Spi::run("SET LOCAL warren.threads = 2")?;
        assert_eq!(workers().map_err(|_| Error::InvalidPosition)?.threads(), 2);
        Ok(())
    }
}
