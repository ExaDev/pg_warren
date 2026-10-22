//! Where the registry lives (DESIGN.md §5.6, §14 item 27): on PG 17 and 18
//! in a named DSM segment that the first backend to use it creates
//! (`GetNamedDSMSegment`), so nothing needs configuring; on PG 15 and 16 in
//! the main shared memory, placed by the `shmem_request_hook` and
//! `shmem_startup_hook` that `_PG_init` installs only while
//! `shared_preload_libraries` loads pg_warren. Without the preload those
//! majors have no registry, and the shared cache is unavailable.

use std::ptr::{addr_of_mut, NonNull};
use std::sync::atomic::{AtomicU32, AtomicU64};

use pgrx::pg_sys;

use crate::cache::shared::registry::Clocks;
use crate::cache::shared::slot::Slot;
use crate::cache::shared::unavailable::Unavailable;

/// The registry's slots: each shared version takes one, and one of
/// Postgres's DSM control slots, so the byte cap, not this, is the bound
/// that matters (DESIGN.md §5.6).
pub(super) const SLOTS: usize = 128;

/// The registry's format, changed whenever its layout or a version
/// segment's does; backends of builds with different formats use
/// different registries.
pub(in crate::cache::shared) const FORMAT: u32 = 3;

/// The name of the registry: its named DSM segment on PG 17 and 18, its
/// `ShmemInitStruct` entry on PG 15 and 16. It ends with [`FORMAT`].
pub(super) const NAME: &std::ffi::CStr = c"pg_warren.cache.3";

/// The LWLock tranche's name, which each backend registers.
const TRANCHE: &std::ffi::CStr = c"pg_warren";

/// The registry in shared memory: its lock (PG 17 and 18; PG 15 and 16
/// take one from a named tranche), clocks, the clocks' drain count as an
/// atomic, slots, pins and a condition variable per slot, broadcast when
/// it leaves `loading`.
#[repr(C)]
pub(super) struct Shared {
    format: u32,
    tranche: i32,
    lock: pg_sys::LWLock,
    pub(super) clocks: Clocks,
    /// `clocks`' drain count, for backends to read without the lock.
    pub(super) drains: AtomicU64,
    pub(super) slots: [Slot; SLOTS],
    pub(super) pins: [AtomicU32; SLOTS],
    pub(super) changed: [pg_sys::ConditionVariable; SLOTS],
}

/// The registry as this backend found it.
pub(super) struct Located {
    pub(super) shared: NonNull<Shared>,
    pub(super) lock: NonNull<pg_sys::LWLock>,
}

/// Sets up the registry at `shared`, whose lock (if it is the registry's
/// own) is in tranche `tranche`.
///
/// # Safety
///
/// `shared` points to `size_of::<Shared>()` writable bytes that no other
/// process uses until this returns.
unsafe fn initialize(shared: *mut Shared, tranche: i32) {
    // SAFETY: the caller's contract: `shared` is writable and ours alone;
    // each field is written whole before anything reads it.
    unsafe {
        addr_of_mut!((*shared).format).write(FORMAT);
        addr_of_mut!((*shared).tranche).write(tranche);
        pg_sys::LWLockInitialize(addr_of_mut!((*shared).lock), tranche);
        addr_of_mut!((*shared).clocks).write(Clocks::NEW);
        addr_of_mut!((*shared).drains).write(AtomicU64::new(0));
        addr_of_mut!((*shared).slots).write([Slot::EMPTY; SLOTS]);
        addr_of_mut!((*shared).pins).write([const { AtomicU32::new(0) }; SLOTS]);
        for slot in 0..SLOTS {
            pg_sys::ConditionVariableInit(addr_of_mut!((*shared).changed[slot]));
        }
    }
}

/// Whether a registry of format `found` is this build's; one of another
/// format (a build that a restart has not yet replaced) cannot be read.
#[cfg(any(feature = "pg17", feature = "pg18", test))]
fn checked(found: u32) -> Result<(), Unavailable> {
    if found == FORMAT {
        Ok(())
    } else {
        Err(Unavailable::OtherFormat { found })
    }
}

/// The size of the first segment of a DSA area: Postgres's DSM registry
/// keeps its index of named segments in one (`dsa_create`), created with
/// the cluster's first named segment.
#[cfg(any(feature = "pg17", feature = "pg18", test))]
const DSA_INITIAL_SEGMENT: usize = 1024 * 1024;

/// The room `GetNamedDSMSegment` may take to create the registry: its own
/// segment, the DSM registry's area if no other extension created it
/// first, and as much again to spare.
#[cfg(any(feature = "pg17", feature = "pg18", test))]
const REGISTRY_ROOM: u64 = (std::mem::size_of::<Shared>() + 2 * DSA_INITIAL_SEGMENT) as u64;

/// Whether the registry may be created where the store (`store`) has
/// `space` (its available bytes and size, `segment::store_space`):
/// without room, `GetNamedDSMSegment` would raise an ERROR where the shared
/// cache must instead be unavailable (DESIGN.md §5.6, Size checks). The
/// room counts the reserve that versions leave free: a cache filled to it
/// must still let new backends find the registry and attach to what it
/// holds, and the registry is created once per cluster.
#[cfg(any(feature = "pg17", feature = "pg18", test))]
fn room_for_registry(space: Option<(u64, u64)>, store: &'static str) -> Result<(), Unavailable> {
    let available = space.map_or(0, |(available, _)| available);
    if available >= REGISTRY_ROOM {
        Ok(())
    } else {
        Err(Unavailable::NoRegistryRoom { store })
    }
}

/// The registry, found or created on first use (PG 17 and 18), unless it
/// has another format or the store has no room to create it.
#[cfg(any(feature = "pg17", feature = "pg18"))]
pub(super) fn locate() -> Result<Located, Unavailable> {
    room_for_registry(super::segment::store_space(), super::segment::store_name())?;
    let mut found = false;
    // SAFETY: called on the main thread inside a transaction; the name is
    // a static C string; `create` sets up a new segment of the size asked
    // before any backend can find it (`GetNamedDSMSegment` holds its lock
    // while it runs). The room was checked above; an ERROR that is left
    // (no DSM control slot, or a race for the store's room) unwinds as
    // usual (DESIGN.md §5.6, Fallback).
    let shared = unsafe {
        let size = std::mem::size_of::<Shared>();
        pg_sys::GetNamedDSMSegment(NAME.as_ptr(), size, Some(create), &mut found)
    };
    // `GetNamedDSMSegment` raises an ERROR rather than return NULL.
    let shared = NonNull::new(shared.cast::<Shared>())
        .unwrap_or_else(|| super::broken("the registry has no address"));
    // SAFETY: the segment holds an initialized registry (`create`), mapped
    // for the rest of the session; its format and tranche are written once
    // (and the format first, at the same offset in every format).
    let (format, tranche) = unsafe { ((*shared.as_ptr()).format, (*shared.as_ptr()).tranche) };
    checked(format)?;
    // SAFETY: registers this registry's tranche under a static name in this
    // backend, as each backend must before it takes the lock.
    unsafe { pg_sys::LWLockRegisterTranche(tranche, TRANCHE.as_ptr()) };
    // SAFETY: `shared` is valid; the lock is its field.
    let lock = unsafe { NonNull::new_unchecked(addr_of_mut!((*shared.as_ptr()).lock)) };
    Ok(Located { shared, lock })
}

/// Sets up a new registry segment (`GetNamedDSMSegment`'s init callback).
#[cfg(any(feature = "pg17", feature = "pg18"))]
#[pgrx::pg_guard]
unsafe extern "C-unwind" fn create(segment: *mut std::ffi::c_void) {
    // SAFETY: a new tranche id, from the main thread.
    let tranche = unsafe { pg_sys::LWLockNewTrancheId() };
    // SAFETY: `GetNamedDSMSegment` passes a new segment of
    // `size_of::<Shared>()` bytes and holds its own lock while this runs.
    unsafe { initialize(segment.cast(), tranche) };
}

/// Nothing to request: on PG 17 and 18 the registry is a named DSM segment.
#[cfg(any(feature = "pg17", feature = "pg18"))]
pub(crate) fn request_preload() {}

#[cfg(any(feature = "pg15", feature = "pg16"))]
pub(super) use preloaded::locate;
#[cfg(any(feature = "pg15", feature = "pg16"))]
pub(crate) use preloaded::request_preload;

/// The registry in the main shared memory, placed while pg_warren is
/// preloaded (PG 15 and 16).
#[cfg(any(feature = "pg15", feature = "pg16"))]
mod preloaded {
    use std::mem::size_of;
    use std::ptr::{addr_of_mut, NonNull};
    use std::sync::atomic::{AtomicPtr, Ordering};

    use pgrx::pg_sys;

    use super::{initialize, Located, Shared, Unavailable, NAME, TRANCHE};

    /// The registry, once the startup hook has placed it: set in the
    /// postmaster, and so in every backend it forks.
    static REGISTRY: AtomicPtr<Shared> = AtomicPtr::new(std::ptr::null_mut());

    /// The hooks installed before pg_warren's, which it calls first.
    static mut PREVIOUS_REQUEST: pg_sys::shmem_request_hook_type = None;
    static mut PREVIOUS_STARTUP: pg_sys::shmem_startup_hook_type = None;

    /// `AddinShmemInitLock`, a macro in C: `&MainLWLockArray[21].lock` on
    /// PG 15 to 18.
    const ADDIN_SHMEM_INIT_LOCK: usize = 21;

    /// Installs the hooks that place the registry, if
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

    /// Requests the registry's memory and its lock.
    #[pgrx::pg_guard]
    unsafe extern "C-unwind" fn request() {
        // SAFETY: Postgres calls this hook once, in the postmaster, while
        // shared memory may be requested; the previous hook is read by value.
        unsafe {
            if let Some(previous) = PREVIOUS_REQUEST {
                previous();
            }
            pg_sys::RequestAddinShmemSpace(size_of::<Shared>());
            pg_sys::RequestNamedLWLockTranche(TRANCHE.as_ptr(), 1);
        }
    }

    /// Places the registry in the main shared memory.
    #[pgrx::pg_guard]
    unsafe extern "C-unwind" fn startup() {
        // SAFETY: Postgres calls this hook once shared memory exists, in the
        // postmaster (and, under EXEC_BACKEND, in each backend);
        // `AddinShmemInitLock` serializes `ShmemInitStruct` and the
        // initialization of what it returns, as the C macro's users do.
        unsafe {
            if let Some(previous) = PREVIOUS_STARTUP {
                previous();
            }
            let init_lock =
                addr_of_mut!((*pg_sys::MainLWLockArray.add(ADDIN_SHMEM_INIT_LOCK)).lock);
            pg_sys::LWLockAcquire(init_lock, pg_sys::LWLockMode::LW_EXCLUSIVE);
            let mut found = false;
            let shared =
                pg_sys::ShmemInitStruct(NAME.as_ptr(), size_of::<Shared>(), &mut found).cast();
            if !found {
                initialize(shared, 0);
            }
            pg_sys::LWLockRelease(init_lock);
            REGISTRY.store(shared, Ordering::Release);
        }
    }

    /// The registry, if the preload placed it (with this build: a
    /// preloaded library cannot change without a restart).
    pub(in crate::cache::shared::dsm) fn locate() -> Result<Located, Unavailable> {
        let shared =
            NonNull::new(REGISTRY.load(Ordering::Acquire)).ok_or(Unavailable::NotPreloaded)?;
        // SAFETY: the tranche was requested with the registry, so it exists
        // in every backend that has the registry; called on the main thread.
        let tranche = unsafe { pg_sys::GetNamedLWLockTranche(TRANCHE.as_ptr()) };
        // SAFETY: the tranche's single lock, in the main lock array.
        let lock = NonNull::new(unsafe { addr_of_mut!((*tranche).lock) })
            .ok_or(Unavailable::NotPreloaded)?;
        Ok(Located { shared, lock })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registrys_name_carries_its_format() {
        assert_eq!(NAME.to_str().ok(), Some(format!("pg_warren.cache.{FORMAT}").as_str()));
    }

    #[test]
    fn a_registry_of_another_format_is_unavailable() {
        assert_eq!(checked(FORMAT), Ok(()));
        assert_eq!(checked(FORMAT + 1), Err(Unavailable::OtherFormat { found: FORMAT + 1 }));
    }

    #[test]
    fn the_registry_is_created_only_where_it_and_the_dsm_registrys_area_fit() {
        const MB: u64 = 1 << 20;
        let unavailable = |store| Err(Unavailable::NoRegistryRoom { store });
        assert!(REGISTRY_ROOM >= (std::mem::size_of::<Shared>() + DSA_INITIAL_SEGMENT) as u64);
        assert_eq!(room_for_registry(Some((REGISTRY_ROOM, 64 * MB)), "/dev/shm"), Ok(()));
        assert_eq!(room_for_registry(Some((u64::MAX, u64::MAX)), "/dev/shm"), Ok(()), "sysv");
        assert_eq!(
            room_for_registry(Some((REGISTRY_ROOM - 1, 64 * MB)), "/dev/shm"),
            unavailable("/dev/shm")
        );
        assert_eq!(room_for_registry(None, "pg_dynshmem"), unavailable("pg_dynshmem"));
    }

    #[test]
    fn a_shared_cache_filled_to_its_reserve_leaves_room_for_the_registry() {
        const MB: u64 = 1 << 20;
        // Docker's 64 MB /dev/shm with 48 MB of shared versions: no room
        // for another version, but the reserve can take the registry, so
        // new backends attach to the shared versions.
        let filled = Some((16 * MB, 64 * MB));
        assert_eq!(
            filled.map(|(available, size)| super::super::segment::beside_reserve(available, size)),
            Some(0)
        );
        assert_eq!(room_for_registry(filled, "/dev/shm"), Ok(()));
    }

    #[test]
    fn the_registry_is_a_few_tens_of_kilobytes() {
        let size = std::mem::size_of::<Shared>();
        assert!(size < 64 * 1024, "{size}");
    }
}
