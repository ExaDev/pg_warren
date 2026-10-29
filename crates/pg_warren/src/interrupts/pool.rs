//! The compute pool (DESIGN.md §6.9): `warren.threads`, its size (default
//! half the cores, at least 1). `warren.max_compute_memory`, the compute
//! memory a call may hold, is defined with the cache's bound, which it
//! shares (`cache::compute_memory`).
//!
//! A call runs on a pool of the threads it is granted (DESIGN.md §14 item
//! 52): it asks for `warren.threads` within `warren.threads_limit`, and
//! gets as many as are free under `warren.cluster_threads_limit`, the cap
//! on the threads of every backend's calls together (`admission`), waiting
//! on the process latch while none is. It holds them until its [`Granted`]
//! pool drops; a call made inside it (a nested `workers()`) shares them.
//! The pool is kept and rebuilt when the grant changes. Its threads block
//! every signal: Postgres's handlers must run on the main thread, which is
//! the only one that may touch backend state.
//!
//! A FATAL raised while a kernel runs (`pg_terminate_backend`) exits the
//! process from the main thread's poll without unwinding, so nothing waits
//! for the pool's threads, which may still read a shared version's
//! segment: Postgres unmaps every segment as it exits, and a thread
//! reading one would crash the backend and restart the cluster. So the
//! first call registers a `before_shmem_exit` callback, which runs before
//! that, stops every pool of the backend still in use (DESIGN.md §6.9) and
//! gives back the threads its call held.

use std::cell::{Cell, RefCell};
use std::ops::Deref;
use std::rc::{Rc, Weak};
use std::sync::atomic::AtomicU32;
use std::thread::available_parallelism;

use pgrx::{pg_sys, GucContext, GucFlags, GucRegistry, GucSetting};
use warren_core::compute::{ComputeError, Workers};

use super::admission::{give_back, take_waiting};
use crate::activity;
use crate::ceiling::{self, Ceiling};

/// The most compute threads a backend may run.
const MAX_THREADS: i32 = 1024;

/// How long a call waiting for threads sleeps between looks, in
/// milliseconds; a cancel wakes it sooner (the latch is set).
const WAIT_MS: std::ffi::c_long = 1;

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
           eigen, distance), and a refresh mapping edge endpoints, run on.",
        c"Defaults to half the cores. Compute threads never call into Postgres. A call runs \
           on fewer where warren.threads_limit is lower, or where fewer are free under \
           warren.cluster_threads_limit.",
        &THREADS,
        1,
        MAX_THREADS,
        GucContext::Userset,
        GucFlags::default(),
    );
}

/// The threads this backend's call holds.
#[derive(Debug, Clone, Copy)]
struct Grant {
    /// The [`Granted`] pools alive: the call's and those of calls nested
    /// in it.
    holders: u32,
    /// The cluster's count and what was taken from it, if the cap applies.
    counted: Option<(&'static AtomicU32, u32)>,
}

impl Grant {
    const NONE: Self = Self { holders: 0, counted: None };
}

thread_local! {
    /// The backend's compute pool, if built.
    static WORKERS: RefCell<Option<Rc<Workers>>> = const { RefCell::new(None) };
    /// Every pool the backend has built, for the exit callback: a pool
    /// replaced after a change of grant may still run a job for a caller
    /// holding it.
    static BUILT: RefCell<Vec<Weak<Workers>>> = const { RefCell::new(Vec::new()) };
    /// The threads the running call holds.
    static GRANT: Cell<Grant> = const { Cell::new(Grant::NONE) };
    /// Whether the exit callback is registered.
    static EXIT_REGISTERED: Cell<bool> = const { Cell::new(false) };
    /// Whether this backend has logged that the cluster's cap is not
    /// enforced.
    static UNCAPPED_LOGGED: Cell<bool> = const { Cell::new(false) };
}

/// A call's compute pool, of the threads it was granted, which are given
/// back when the call's last [`Granted`] drops.
pub(crate) struct Granted(Rc<Workers>);

impl Deref for Granted {
    type Target = Workers;

    fn deref(&self) -> &Workers {
        &self.0
    }
}

impl Drop for Granted {
    fn drop(&mut self) {
        release();
    }
}

/// Ends a holder of the call's threads: the last gives them back. Pure
/// Rust (an atomic), so it may run as an ERROR unwinds.
fn release() {
    let grant = GRANT.get();
    let holders = grant.holders.saturating_sub(1);
    if holders == 0 {
        if let Some((count, threads)) = grant.counted {
            give_back(count, threads);
        }
        GRANT.set(Grant::NONE);
    } else {
        GRANT.set(Grant { holders, ..grant });
    }
}

/// The call's compute pool: of the threads granted under the ceilings and
/// the cluster's cap (waiting while none is free), or, inside a call that
/// holds a pool, that one. On the main thread, inside a call.
pub(crate) fn workers() -> Result<Granted, ComputeError> {
    register_exit();
    let asked = Ceiling::Threads.bound(u64::try_from(THREADS.get()).unwrap_or(1)).value;
    let asked = u32::try_from(asked).unwrap_or(1).max(1);
    let count = match ceiling::cluster_threads() {
        Some(cap) => cluster_count().map(|count| (count, cap)),
        None => None,
    };
    workers_counted(asked, count, wait_for_threads)
}

/// [`workers`], asking for `asked` threads, of `count` under its cap if
/// the cap applies, calling `wait` while none is free.
fn workers_counted(
    asked: u32,
    count: Option<(&'static AtomicU32, u32)>,
    wait: impl FnMut(),
) -> Result<Granted, ComputeError> {
    let grant = GRANT.get();
    if grant.holders > 0 {
        if let Some(workers) = WORKERS.with_borrow(Option::clone) {
            GRANT.set(Grant { holders: grant.holders + 1, ..grant });
            return Ok(Granted(workers));
        }
    }
    let (threads, counted) = match count {
        Some((count, cap)) => {
            let taken = take_waiting(count, asked, cap, wait);
            (taken, Some((count, taken)))
        }
        None => (asked, None),
    };
    GRANT.set(Grant { holders: 1, counted });
    match pool(threads as usize) {
        Ok(workers) => Ok(Granted(workers)),
        Err(error) => {
            release();
            Err(error)
        }
    }
}

/// The backend's pool of `threads`, built (or rebuilt) when its size
/// differs.
fn pool(threads: usize) -> Result<Rc<Workers>, ComputeError> {
    if let Some(workers) = WORKERS.with_borrow(|w| w.clone().filter(|w| w.threads() == threads)) {
        return Ok(workers);
    }
    let workers = Rc::new(with_signals_blocked(|| Workers::new(threads))?);
    WORKERS.set(Some(Rc::clone(&workers)));
    BUILT.with_borrow_mut(|built| {
        built.retain(|pool| pool.strong_count() > 0);
        built.push(Rc::downgrade(&workers));
    });
    Ok(workers)
}

/// The cluster's count of compute threads, if this backend has
/// pg_warren's shared memory; otherwise the cap is not enforced, which the
/// backend logs once.
fn cluster_count() -> Option<&'static AtomicU32> {
    let count = activity::compute_threads();
    if count.is_none() && !UNCAPPED_LOGGED.replace(true) {
        pgrx::ereport!(
            pgrx::PgLogLevel::LOG,
            pgrx::PgSqlErrorCode::ERRCODE_SUCCESSFUL_COMPLETION,
            "warren.cluster_threads_limit is not enforced in this backend: pg_warren's shared \
             memory is not available",
            "On PostgreSQL 15 and 16 it needs shared_preload_libraries = 'pg_warren'."
        );
    }
    count
}

/// Sleeps on the process latch until it is set or a moment has passed,
/// then checks for interrupts: a cancel, `statement_timeout` or
/// termination ends the wait for threads with its ERROR (or FATAL).
fn wait_for_threads() {
    let events = pg_sys::WL_LATCH_SET | pg_sys::WL_TIMEOUT | pg_sys::WL_EXIT_ON_PM_DEATH;
    let event = activity::wait_event(c"WarrenComputeThreads");
    // SAFETY: the process latch, on the main thread; a signal sets it.
    unsafe {
        pg_sys::WaitLatch(pg_sys::MyLatch, events as i32, WAIT_MS, event);
        pg_sys::ResetLatch(pg_sys::MyLatch);
    }
    pgrx::check_for_interrupts!();
}

/// Registers the exit callback, once per backend.
fn register_exit() {
    if !EXIT_REGISTERED.replace(true) {
        // SAFETY: on the main thread, once per backend; the callback is a
        // function of the library.
        unsafe { pg_sys::before_shmem_exit(Some(stop_at_exit), pg_sys::Datum::from(0)) };
    }
}

/// As the backend exits, cancels the kernel its pools run, if any, waits
/// for their threads, and gives back the threads its call held. Pure
/// Rust, as an exit callback must be.
#[pgrx::pg_guard]
unsafe extern "C-unwind" fn stop_at_exit(_code: i32, _arg: pg_sys::Datum) {
    if let Ok(built) = BUILT.try_with(|built| built.try_borrow().map(|b| b.clone())) {
        for pool in built.into_iter().flatten().filter_map(|pool| pool.upgrade()) {
            pool.stop();
        }
    }
    if let Ok(Grant { counted: Some((count, threads)), .. }) = GRANT.try_with(Cell::get) {
        give_back(count, threads);
        let _ = GRANT.try_with(|grant| grant.set(Grant::NONE));
    }
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
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU32, Ordering};

    use pgrx::prelude::*;
    use pgrx::spi::Error;
    use warren_core::interrupts::Uninterrupted;

    use super::{default_threads, take_waiting, wait_for_threads, workers, workers_counted};
    use crate::ceiling::set_in_configuration;

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
        drop(pool);
        Spi::run("SET LOCAL warren.threads = 2")?;
        assert_eq!(workers().map_err(|_| Error::InvalidPosition)?.threads(), 2);
        Ok(())
    }

    #[pg_test]
    fn warren_threads_limit_bounds_the_pool() -> Result<(), Error> {
        Spi::run("SET LOCAL warren.threads = 3")?;
        set_in_configuration(c"warren.threads_limit", c"2");
        let threads = workers().map(|pool| pool.threads());
        set_in_configuration(c"warren.threads_limit", c"-1");
        assert_eq!(threads.ok(), Some(2));
        Ok(())
    }

    /// A cluster's count of its own, for tests: the shared one is used by
    /// every test's backend.
    fn count(held: u32) -> &'static AtomicU32 {
        Box::leak(Box::new(AtomicU32::new(held)))
    }

    #[pg_test]
    fn a_call_runs_on_the_threads_free_and_gives_them_back() -> Result<(), Error> {
        let cluster = count(3);
        // A wait would be a failure: it records itself and empties the
        // count, so the test ends rather than hangs.
        let waits = Cell::new(0);
        let wait = || {
            waits.set(waits.get() + 1);
            cluster.store(0, Ordering::Relaxed);
        };
        let pool =
            workers_counted(4, Some((cluster, 5)), wait).map_err(|_| Error::InvalidPosition)?;
        assert_eq!(pool.threads(), 2, "two of five free");
        assert_eq!(cluster.load(Ordering::Relaxed), 5);
        let nested =
            workers_counted(4, Some((cluster, 5)), wait).map_err(|_| Error::InvalidPosition)?;
        assert_eq!(nested.threads(), 2, "a nested call shares its caller's");
        drop(nested);
        assert_eq!(cluster.load(Ordering::Relaxed), 5, "held until the call's pool drops");
        drop(pool);
        assert_eq!(cluster.load(Ordering::Relaxed), 3);
        let uncapped = workers_counted(4, None, wait).map_err(|_| Error::InvalidPosition)?;
        assert_eq!(uncapped.threads(), 4);
        assert_eq!(waits.get(), 0, "no call waited");
        Ok(())
    }

    /// Waits for a thread of a full cluster, marking a cancel pending
    /// before its first sleep, as `pg_cancel_backend` would mid-wait; the
    /// count after, through `held`.
    #[pg_extern]
    fn wait_for_a_full_cluster() {
        let cluster = count(2);
        HELD.store(u32::MAX, Ordering::Relaxed);
        let _ = take_waiting(cluster, 1, 2, || {
            HELD.store(cluster.load(Ordering::Relaxed), Ordering::Relaxed);
            // SAFETY: sets the flags the SIGINT handler sets, on the main
            // thread; the wait's check processes and clears them.
            unsafe {
                pg_sys::QueryCancelPending = 1;
                pg_sys::InterruptPending = 1;
            }
            wait_for_threads();
        });
    }

    static HELD: AtomicU32 = AtomicU32::new(0);

    #[pg_test]
    fn a_cancel_ends_the_wait_for_threads() -> Result<(), Error> {
        let canceled = Spi::get_one::<bool>(
            "DO $do$ BEGIN
                 PERFORM tests.wait_for_a_full_cluster();
                 RAISE EXCEPTION 'not cancelled';
             EXCEPTION WHEN query_canceled THEN NULL;
             END $do$;
             SELECT true",
        )?;
        assert_eq!(canceled, Some(true));
        assert_eq!(HELD.load(Ordering::Relaxed), 2, "it waited while none was free");
        Ok(())
    }
}
