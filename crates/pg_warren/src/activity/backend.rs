//! This backend's side of the activity table (DESIGN.md §5.7): its own
//! copy of what its slot holds, which every write changes first, and the
//! slot it claimed, which each write then publishes. Writes are pure Rust
//! (atomics, no lock), so the cache may record at a statement's end, inside
//! `AtCommit_Memory`. The slot is claimed, under the table's lock, at the
//! backend's first use of pg_warren (`claim`, from a call), and given back
//! from its `before_shmem_exit` callback, its counters added to the totals.
//! Without a table (PG 15 and 16 without the preload) the copy is all
//! there is, and `warren.backends()` shows only it.

use std::cell::RefCell;

use pgrx::pg_sys;

use super::placement::{self, Located, Missing};
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
    /// There is no table for it.
    Missing(Missing),
}

thread_local! {
    static BACKEND: RefCell<Backend> =
        const { RefCell::new(Backend { values: Values::FREE, table: Claimed::Not }) };
}

/// Changes this backend's values through `change`, then publishes them to
/// its slot, if it has one. Pure Rust: legal at a statement's end.
pub(crate) fn record(change: impl FnOnce(&mut Values)) {
    BACKEND.with(|backend| {
        let Ok(mut backend) = backend.try_borrow_mut() else {
            return;
        };
        change(&mut backend.values);
        if let Claimed::Slot { table, index } = backend.table {
            table.table().write(index, &backend.values);
        }
    });
}

/// This backend's values, as its slot shows them.
pub(crate) fn own() -> Values {
    BACKEND.with_borrow(|backend| backend.values)
}

/// Claims this backend's slot, the first time it uses pg_warren, and
/// registers the callback that gives it back at exit; looks again for a
/// table that had no room. On the main thread, inside a call.
pub(crate) fn claim() {
    let table = BACKEND.with_borrow(|backend| backend.table);
    let look = match table {
        Claimed::Not => true,
        Claimed::Missing(missing) => missing.looked_for_again(),
        Claimed::Slot { .. } => false,
    };
    if look {
        let claimed = match placement::locate() {
            Ok(table) => claim_in(table),
            Err(missing) => Claimed::Missing(missing),
        };
        BACKEND.with_borrow_mut(|backend| backend.table = claimed);
    }
}

/// The table, if this backend has one: its slot's index and the table's
/// location; else why not.
pub(super) fn located() -> Result<(Located, usize), Option<Missing>> {
    match BACKEND.with_borrow(|backend| backend.table) {
        Claimed::Slot { table, index } => Ok((table, index)),
        Claimed::Missing(missing) => Err(Some(missing)),
        Claimed::Not => Err(None),
    }
}

/// Claims this backend's slot in `table`, publishing what it has recorded
/// so far.
fn claim_in(table: Located) -> Claimed {
    let Some(index) = proc_number() else {
        return Claimed::Missing(Missing::NoSlot);
    };
    let holder = holder();
    let values = BACKEND.with_borrow_mut(|backend| {
        backend.values.holder = holder;
        backend.values
    });
    if !table.table().claim(index, holder, &values) {
        return Claimed::Missing(Missing::NoSlot);
    }
    // SAFETY: on the main thread, once per backend (the slot is claimed
    // once); the callback lives as long as the library.
    unsafe { pg_sys::before_shmem_exit(Some(exit), pg_sys::Datum::from(0)) };
    Claimed::Slot { table, index }
}

/// Who this backend is: its pid, database, session user and start.
pub(super) fn holder() -> Holder {
    // SAFETY: the backend's identity, set before any query, read on the
    // main thread.
    unsafe {
        Holder {
            pid: pg_sys::MyProcPid,
            database: pg_sys::MyDatabaseId,
            role: pg_sys::GetSessionUserId(),
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
