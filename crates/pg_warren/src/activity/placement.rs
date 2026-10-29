//! Where the activity table lives (DESIGN.md §5.7, §14 item 48), located
//! as the shared cache's registry is (§14 item 27): on PG 17 and 18 a named
//! DSM segment that the first backend to use pg_warren creates, so nothing
//! needs configuring; on PG 15 and 16 the main shared memory, placed by the
//! hooks `_PG_init` installs only while `shared_preload_libraries` loads
//! pg_warren. Its size follows `MaxBackends`, fixed at startup. A crash
//! restart reinitialises it, as it does the registry.
//!
//! | Part | Contents |
//! |---|---|
//! | header | format, slot count, creation time, the counters of exited backends, the compute threads held (since format 2); on PG 17 and 18 the LWLock and its tranche (on 15 and 16 the lock is the preload's named tranche's) |
//! | slots | one per backend, `MaxBackends` of them |

use std::mem::{align_of, size_of};
use std::ptr::{addr_of_mut, NonNull};
use std::sync::atomic::AtomicU32;

use pgrx::pg_sys;

use super::change_count::Counted;
use super::slot::{Slot, COUNTER_WORDS};
use super::table::{Lock, Table};

/// The table's format, changed whenever its layout does; builds with
/// different formats use different tables.
const FORMAT: u32 = 2;

/// The table's name: its named DSM segment on PG 17 and 18, its
/// `ShmemInitStruct` entry on PG 15 and 16. It ends with [`FORMAT`].
const NAME: &std::ffi::CStr = c"pg_warren.activity.2";

/// The LWLock tranche's name, which each backend registers.
const TRANCHE: &std::ffi::CStr = c"pg_warren.activity";

/// The table's header in shared memory; its slots follow it.
#[repr(C)]
struct Header {
    format: u32,
    #[cfg(any(feature = "pg17", feature = "pg18"))]
    tranche: i32,
    slots: u64,
    /// When the table was created, as a `timestamptz`.
    created: i64,
    #[cfg(any(feature = "pg17", feature = "pg18"))]
    lock: pg_sys::LWLock,
    exited: Counted<COUNTER_WORDS>,
    /// The compute threads every backend's calls hold (DESIGN.md §14 item
    /// 52), which each backend changes for itself.
    compute_threads: AtomicU32,
}

/// The bytes before the first slot.
const SLOTS_AT: usize = size_of::<Header>().next_multiple_of(align_of::<Slot>());

/// The bytes of a table of `slots` slots.
fn table_bytes(slots: usize) -> usize {
    SLOTS_AT + slots * size_of::<Slot>()
}

/// The table as this backend found it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Located {
    header: NonNull<Header>,
    lock: NonNull<pg_sys::LWLock>,
}

/// The table's LWLock, held around pure Rust only.
pub(super) struct LwLock(NonNull<pg_sys::LWLock>);

impl Lock for LwLock {
    fn exclusive<R>(&self, f: impl FnOnce() -> R) -> R {
        // SAFETY: the table's lock, on the main thread; LWLockAcquire holds
        // interrupts off while it waits, and `f` is pure Rust.
        unsafe { pg_sys::LWLockAcquire(self.0.as_ptr(), pg_sys::LWLockMode::LW_EXCLUSIVE) };
        let result = f();
        // SAFETY: released as it was taken.
        unsafe { pg_sys::LWLockRelease(self.0.as_ptr()) };
        result
    }
}

impl Located {
    /// The table, mapped for the rest of the session.
    pub(super) fn table(&self) -> Table<'static, LwLock> {
        let header = self.header.as_ptr();
        // SAFETY: the header and its slots stay mapped for the session (a
        // named segment, or the main shared memory); the slot count is
        // written once, before any backend can find the table; the exited
        // counters and slots are atomics, read and written through shared
        // references.
        unsafe {
            let count = (*header).slots as usize;
            let slots = header.cast::<u8>().add(SLOTS_AT).cast::<Slot>();
            Table {
                lock: LwLock(self.lock),
                exited: &(*header).exited,
                slots: std::slice::from_raw_parts(slots, count),
            }
        }
    }

    /// The count of compute threads that every backend's calls hold.
    pub(super) fn compute_threads(&self) -> &'static AtomicU32 {
        // SAFETY: the header stays mapped for the session; the count is an
        // atomic, read and written through a shared reference.
        unsafe { &(*self.header.as_ptr()).compute_threads }
    }

    /// When the table was created, as a `timestamptz`.
    pub(super) fn created(&self) -> i64 {
        // SAFETY: written once, before any backend can find the table.
        unsafe { (*self.header.as_ptr()).created }
    }
}

/// Sets up a table of `slots` slots at `header`, but for its lock.
///
/// # Safety
///
/// `header` points to `table_bytes(slots)` writable bytes, aligned for
/// `Header`, that no other process uses until this returns; on the main
/// thread.
unsafe fn initialize(header: *mut Header, slots: usize) {
    // SAFETY: the caller's contract; each field is written whole before
    // anything reads it, and every slot is zeroed, which is free.
    unsafe {
        addr_of_mut!((*header).format).write(FORMAT);
        addr_of_mut!((*header).slots).write(slots as u64);
        addr_of_mut!((*header).created).write(pg_sys::GetCurrentTimestamp());
        addr_of_mut!((*header).exited).write(Counted::zero());
        addr_of_mut!((*header).compute_threads).write(AtomicU32::new(0));
        let first = header.cast::<u8>().add(SLOTS_AT).cast::<Slot>();
        for slot in 0..slots {
            first.add(slot).write(Slot::zero());
        }
    }
}

/// The number of slots: one per possible backend.
fn max_backends() -> usize {
    // SAFETY: computed before `shmem_request_hook` runs and fixed after.
    usize::try_from(unsafe { pg_sys::MaxBackends }).unwrap_or(0)
}

/// Why this backend has no activity table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Missing {
    /// PG 15 or 16 without `shared_preload_libraries = 'pg_warren'`.
    #[cfg(any(feature = "pg15", feature = "pg16"))]
    NotPreloaded,
    /// PG 17 or 18, where the store had too little room to create it in
    /// the statement that started at `statement`; it is looked for again
    /// in the next statement that uses pg_warren.
    #[cfg(any(feature = "pg17", feature = "pg18"))]
    NoRoom { statement: i64 },
    /// A table of another format: a build that a restart has not yet
    /// replaced created it.
    #[cfg(any(feature = "pg17", feature = "pg18"))]
    OtherFormat,
    /// This process has no index among `MaxBackends` (Postgres gives every
    /// backend and worker one, so this is a belt).
    NoSlot,
    /// The backend has given its slot back, as it exits.
    Exited,
}

impl Missing {
    /// Whether a use in the statement that started at `statement` looks
    /// for the table again: only where it had no room, which may have been
    /// made since, and at most once a statement, since looking reads the
    /// store's free space and takes the DSM registry's lock.
    #[cfg(any(feature = "pg17", feature = "pg18"))]
    pub(super) fn looked_for_again(self, statement: i64) -> bool {
        matches!(self, Self::NoRoom { statement: looked } if looked != statement)
    }

    /// As above: on PG 15 and 16 the table is never looked for again.
    #[cfg(any(feature = "pg15", feature = "pg16"))]
    pub(super) fn looked_for_again(self, _statement: i64) -> bool {
        false
    }
}

/// When the current statement started, as a `timestamptz`.
pub(super) fn statement_start() -> i64 {
    // SAFETY: reads the transaction state's timestamp; inside a call.
    unsafe { pg_sys::GetCurrentStatementStartTimestamp() }
}

/// The table, found or created on first use (PG 17 and 18), unless the
/// store has no room to create it or it has another format.
#[cfg(any(feature = "pg17", feature = "pg18"))]
pub(super) fn locate() -> Result<Located, Missing> {
    let bytes = table_bytes(max_backends());
    let room = crate::shared_memory::named_segment_room(bytes);
    let available = crate::shared_memory::store_space().map_or(0, |(available, _)| available);
    if available < room {
        return Err(Missing::NoRoom { statement: statement_start() });
    }
    let mut found = false;
    // SAFETY: on the main thread inside a transaction; the name is a
    // static C string; `create` sets up a new segment of the size asked
    // before any backend can find it (`GetNamedDSMSegment` holds its lock
    // while it runs). The room was checked above.
    let header =
        unsafe { pg_sys::GetNamedDSMSegment(NAME.as_ptr(), bytes, Some(create), &mut found) };
    let Some(header) = NonNull::new(header.cast::<Header>()) else {
        return Err(Missing::NoRoom { statement: statement_start() });
    };
    // SAFETY: the segment holds an initialized table, mapped for the rest
    // of the session; its format and tranche are written once.
    let (format, tranche) = unsafe { ((*header.as_ptr()).format, (*header.as_ptr()).tranche) };
    if format != FORMAT {
        return Err(Missing::OtherFormat);
    }
    // SAFETY: registers the table's tranche under a static name in this
    // backend, as each backend must before it takes the lock.
    unsafe { pg_sys::LWLockRegisterTranche(tranche, TRANCHE.as_ptr()) };
    // SAFETY: `header` is valid; the lock is its field.
    let lock = unsafe { NonNull::new_unchecked(addr_of_mut!((*header.as_ptr()).lock)) };
    Ok(Located { header, lock })
}

/// Sets up a new table segment (`GetNamedDSMSegment`'s init callback).
#[cfg(any(feature = "pg17", feature = "pg18"))]
#[pgrx::pg_guard]
unsafe extern "C-unwind" fn create(segment: *mut std::ffi::c_void) {
    // SAFETY: a new tranche id, from the main thread.
    let tranche = unsafe { pg_sys::LWLockNewTrancheId() };
    let header = segment.cast::<Header>();
    // SAFETY: `GetNamedDSMSegment` passes a new, page-aligned segment of
    // `table_bytes(max_backends())` bytes and holds its own lock while this
    // runs; the lock and its tranche are written whole before any backend
    // can find the segment.
    unsafe {
        initialize(header, max_backends());
        addr_of_mut!((*header).tranche).write(tranche);
        pg_sys::LWLockInitialize(addr_of_mut!((*header).lock), tranche);
    }
}

/// Nothing to request: on PG 17 and 18 the table is a named DSM segment.
#[cfg(any(feature = "pg17", feature = "pg18"))]
pub(crate) fn request_preload() {}

#[cfg(any(feature = "pg15", feature = "pg16"))]
pub(super) use preloaded::locate;
#[cfg(any(feature = "pg15", feature = "pg16"))]
pub(crate) use preloaded::request_preload;

/// The table in the main shared memory, placed while pg_warren is
/// preloaded (PG 15 and 16).
#[cfg(any(feature = "pg15", feature = "pg16"))]
mod preloaded {
    use std::ptr::{addr_of_mut, NonNull};
    use std::sync::atomic::{AtomicPtr, Ordering};

    use pgrx::pg_sys;

    use super::{initialize, max_backends, table_bytes, Header, Located, Missing, NAME, TRANCHE};

    /// The table, once the startup hook has placed it: set in the
    /// postmaster, and so in every backend it forks.
    static TABLE: AtomicPtr<Header> = AtomicPtr::new(std::ptr::null_mut());

    /// The hooks installed before these, which they call first.
    static mut PREVIOUS_REQUEST: pg_sys::shmem_request_hook_type = None;
    static mut PREVIOUS_STARTUP: pg_sys::shmem_startup_hook_type = None;

    /// `AddinShmemInitLock`, a macro in C: `&MainLWLockArray[21].lock` on
    /// PG 15 to 18.
    const ADDIN_SHMEM_INIT_LOCK: usize = 21;

    /// Installs the hooks that place the table, if
    /// `shared_preload_libraries` is loading pg_warren (from `_PG_init`).
    pub(crate) fn request_preload() {
        // SAFETY: `_PG_init` runs once, on the main thread of the
        // postmaster (or of a backend, where this flag is false); the hooks
        // are read and written only here and by Postgres, in that process.
        unsafe {
            if !pg_sys::process_shared_preload_libraries_in_progress {
                return;
            }
            PREVIOUS_REQUEST = pg_sys::shmem_request_hook;
            pg_sys::shmem_request_hook = Some(request);
            PREVIOUS_STARTUP = pg_sys::shmem_startup_hook;
            pg_sys::shmem_startup_hook = Some(startup);
        }
    }

    /// Requests the table's memory and its lock.
    #[pgrx::pg_guard]
    unsafe extern "C-unwind" fn request() {
        // SAFETY: Postgres calls this hook once, in the postmaster, while
        // shared memory may be requested, after it has computed
        // `MaxBackends`; the previous hook is read by value.
        unsafe {
            if let Some(previous) = PREVIOUS_REQUEST {
                pg_sys::ffi::pg_guard_ffi_boundary(|| previous());
            }
            pg_sys::RequestAddinShmemSpace(table_bytes(max_backends()));
            pg_sys::RequestNamedLWLockTranche(TRANCHE.as_ptr(), 1);
        }
    }

    /// Places the table in the main shared memory.
    #[pgrx::pg_guard]
    unsafe extern "C-unwind" fn startup() {
        // SAFETY: Postgres calls this hook once shared memory exists, in the
        // postmaster (and, under EXEC_BACKEND, in each backend);
        // `AddinShmemInitLock` serializes `ShmemInitStruct` and the
        // initialization of what it returns, as the C macro's users do.
        unsafe {
            if let Some(previous) = PREVIOUS_STARTUP {
                pg_sys::ffi::pg_guard_ffi_boundary(|| previous());
            }
            let init_lock =
                addr_of_mut!((*pg_sys::MainLWLockArray.add(ADDIN_SHMEM_INIT_LOCK)).lock);
            pg_sys::LWLockAcquire(init_lock, pg_sys::LWLockMode::LW_EXCLUSIVE);
            let mut found = false;
            let slots = max_backends();
            let header =
                pg_sys::ShmemInitStruct(NAME.as_ptr(), table_bytes(slots), &mut found).cast();
            if !found {
                initialize(header, slots);
            }
            pg_sys::LWLockRelease(init_lock);
            TABLE.store(header, Ordering::Release);
        }
    }

    /// The table, if the preload placed it.
    pub(in crate::activity) fn locate() -> Result<Located, Missing> {
        let header = NonNull::new(TABLE.load(Ordering::Acquire)).ok_or(Missing::NotPreloaded)?;
        // SAFETY: the tranche was requested with the table, so it exists in
        // every backend that has the table; called on the main thread.
        let tranche = unsafe { pg_sys::GetNamedLWLockTranche(TRANCHE.as_ptr()) };
        // SAFETY: the tranche's single lock, in the main lock array.
        let lock =
            NonNull::new(unsafe { addr_of_mut!((*tranche).lock) }).ok_or(Missing::NotPreloaded)?;
        Ok(Located { header, lock })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(feature = "pg17", feature = "pg18"))]
    #[test]
    fn a_table_without_room_is_looked_for_again_once_per_statement() {
        let missing = Missing::NoRoom { statement: 100 };
        assert!(!missing.looked_for_again(100), "not again in the same statement");
        assert!(missing.looked_for_again(200), "again in the next");
        assert!(!Missing::OtherFormat.looked_for_again(200));
    }

    #[test]
    fn the_tables_name_carries_its_format() {
        assert_eq!(NAME.to_str().ok(), Some(format!("pg_warren.activity.{FORMAT}").as_str()));
    }

    #[test]
    fn the_table_is_about_half_a_megabyte_at_a_thousand_backends() {
        assert_eq!(SLOTS_AT % align_of::<Slot>(), 0);
        let bytes = table_bytes(1000);
        assert!(bytes < 600 * 1024, "{bytes}");
        assert_eq!(table_bytes(2) - table_bytes(1), size_of::<Slot>());
    }
}
