//! This backend's side of the activity table (DESIGN.md §5.7): its own
//! copy of what its slot holds, which every write changes first, and the
//! slot it claimed, which each write then publishes. Writes are pure Rust
//! (atomics, no lock), so the cache may record at a statement's end, inside
//! `AtCommit_Memory`. The slot is claimed, under the table's lock, at the
//! backend's first use of pg_warren (`claim`, from a call), and given back
//! from its `before_shmem_exit` callback, its counters added to the totals.
//! Without a table (PG 15 and 16 without the preload) the copy is all
//! there is, and `warren.backends()` shows only it. A parallel worker
//! claims no slot (§5.7: it reports nothing), but reads the table.

use std::cell::{Cell, RefCell};
use std::sync::atomic::AtomicU32;

use pgrx::pg_sys;

use super::placement::{self, statement_start, Located, Missing};
use super::slot::{Holder, Values};

/// This backend's activity.
struct Backend {
    values: Values,
    table: Claimed,
}

/// Whether this backend has claimed its slot.
#[derive(Debug, Clone, Copy)]
enum Claimed {
    /// Not yet looked for.
    Not,
    /// Its slot, `index`, in the table.
    Slot { table: Located, index: usize },
    /// The table, which a parallel worker reads but holds no slot in.
    Reading(Located),
    /// There is no table for it.
    Missing(Missing),
}

thread_local! {
    static BACKEND: RefCell<Backend> =
        const { RefCell::new(Backend { values: Values::FREE, table: Claimed::Not }) };
    /// Whether the callback that gives the slot back is registered.
    static EXIT_REGISTERED: Cell<bool> = const { Cell::new(false) };
}

/// Changes this backend's values through `change`, then publishes them to
/// its slot, if it has one and they changed. Pure Rust: legal at a
/// statement's end.
pub(crate) fn record(change: impl FnOnce(&mut Values)) {
    BACKEND.with(|backend| {
        let Ok(mut backend) = backend.try_borrow_mut() else {
            return;
        };
        let before = backend.values;
        change(&mut backend.values);
        if let Claimed::Slot { table, index } = backend.table {
            table.table().update(index, &before, &backend.values);
        }
    });
}

/// This backend's values, as its slot shows them.
pub(crate) fn own() -> Values {
    BACKEND.with_borrow(|backend| backend.values)
}

/// Claims this backend's slot, the first time it uses pg_warren, and
/// registers the callback that gives it back at exit; looks again, once
/// per statement, for a table that had no room. A parallel worker only
/// finds the table. On the main thread, inside a call.
pub(crate) fn claim() {
    let table = BACKEND.with_borrow(|backend| backend.table);
    let look = match table {
        Claimed::Not => true,
        Claimed::Missing(missing) => missing.looked_for_again(statement_start()),
        Claimed::Slot { .. } | Claimed::Reading(_) => false,
    };
    if look {
        let claimed = match placement::locate() {
            Ok(table) if parallel_worker() => Claimed::Reading(table),
            Ok(table) => claim_in(table),
            Err(missing) => Claimed::Missing(missing),
        };
        BACKEND.with_borrow_mut(|backend| backend.table = claimed);
    }
}

/// The table, if this backend has found it; else why not.
pub(super) fn located() -> Result<Located, Option<Missing>> {
    match BACKEND.with_borrow(|backend| backend.table) {
        Claimed::Slot { table, .. } | Claimed::Reading(table) => Ok(table),
        Claimed::Missing(missing) => Err(Some(missing)),
        Claimed::Not => Err(None),
    }
}

/// The count of compute threads that every backend's calls hold, in the
/// table's header, if this backend has the table (claiming its slot, the
/// first time). On the main thread, inside a call.
pub(crate) fn compute_threads() -> Option<&'static AtomicU32> {
    claim();
    located().ok().map(|table| table.compute_threads())
}

/// Records the compute threads this backend's running call holds, 0 once
/// it gives them back. Pure Rust: legal as an ERROR unwinds the call.
pub(crate) fn record_compute_threads(threads: u32) {
    record(|values| values.compute_threads = threads);
}

/// Claims this backend's slot in `table`, publishing what it has recorded
/// so far. The callback that gives it back is registered first, so a
/// registration that fails leaves no slot claimed.
fn claim_in(table: Located) -> Claimed {
    let Some(index) = proc_number() else {
        return Claimed::Missing(Missing::NoSlot);
    };
    if !EXIT_REGISTERED.get() {
        // SAFETY: on the main thread, once per backend; the callback lives
        // as long as the library, and gives back only a claimed slot.
        unsafe { pg_sys::before_shmem_exit(Some(exit), pg_sys::Datum::from(0)) };
        EXIT_REGISTERED.set(true);
    }
    let holder = holder();
    let values = BACKEND.with_borrow_mut(|backend| {
        backend.values.holder = holder;
        backend.values
    });
    if !table.table().claim(index, holder, &values) {
        return Claimed::Missing(Missing::NoSlot);
    }
    Claimed::Slot { table, index }
}

/// Whether this process is a parallel worker (`IsParallelWorker()`).
fn parallel_worker() -> bool {
    // SAFETY: set as the worker starts, read on the main thread.
    unsafe { pg_sys::ParallelWorkerNumber >= 0 }
}

/// Between a reader's tries of a slot or of the totals, while their writer
/// writes: spins, and every [`TRIES_PER_PAUSE`] tries checks for
/// interrupts and sleeps a millisecond, so that a reader neither burns a
/// core nor outlives a cancel.
pub(super) fn between_tries() -> impl FnMut() {
    let mut tries: u32 = 0;
    move || {
        tries = tries.wrapping_add(1);
        if tries.is_multiple_of(TRIES_PER_PAUSE) {
            pgrx::check_for_interrupts!();
            // SAFETY: sleeps this process; interruptible by signals.
            unsafe { pg_sys::pg_usleep(1000) };
        } else {
            std::hint::spin_loop();
        }
    }
}

/// How many tries a reader spins before it pauses.
const TRIES_PER_PAUSE: u32 = 1024;

/// Who this backend is: its pid, database, authenticated user and start.
/// The authenticated user is `pg_stat_activity`'s `usesysid`, which `SET
/// SESSION AUTHORIZATION` does not change, so that a backend is seen as
/// `pg_stat_activity` shows it.
pub(super) fn holder() -> Holder {
    // SAFETY: the backend's identity, set before any query, read on the
    // main thread.
    unsafe {
        Holder {
            pid: pg_sys::MyProcPid,
            database: pg_sys::MyDatabaseId,
            role: pg_sys::GetAuthenticatedUserId(),
            started: pg_sys::MyStartTimestamp,
        }
    }
}

/// This backend's index among `MaxBackends`: `MyProcNumber` on PG 17 and
/// 18, `MyBackendId` − 1 on PG 15 and 16.
fn proc_number() -> Option<usize> {
    // SAFETY: set as the process starts, read on the main thread.
    #[cfg(any(feature = "pg17", feature = "pg18"))]
    let index = unsafe { pg_sys::MyProcNumber };
    // SAFETY: as above.
    #[cfg(any(feature = "pg15", feature = "pg16"))]
    let index = unsafe { pg_sys::MyBackendId } - 1;
    usize::try_from(index).ok()
}

/// As the backend exits, gives its slot back, adding its counters to the
/// totals of exited backends.
#[pgrx::pg_guard]
unsafe extern "C-unwind" fn exit(_code: i32, _arg: pg_sys::Datum) {
    BACKEND.with(|backend| {
        let Ok(mut backend) = backend.try_borrow_mut() else {
            return;
        };
        if let Claimed::Slot { table, index } = backend.table {
            table.table().release(index, &backend.values.counters);
            backend.table = Claimed::Missing(Missing::Exited);
        }
    });
}
