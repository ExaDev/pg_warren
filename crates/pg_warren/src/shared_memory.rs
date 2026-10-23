//! Where Postgres creates dynamic shared memory segments, and the room
//! left there (DESIGN.md §5.6, Size checks): `/dev/shm` for `posix`
//! segments (reserved with `posix_fallocate`, so a short store is an ERROR
//! at creation, not a later SIGBUS), `pg_dynshmem` for `mmap` ones,
//! unbounded for `sysv`. On PG 17 and 18 the shared cache's registry and
//! the activity table (§5.7) are named DSM segments, which
//! `GetNamedDSMSegment` creates or raises an ERROR, so each is created
//! only where this says it has room. Depends on no other feature.

use std::ffi::CStr;
use std::mem::MaybeUninit;

use pgrx::pg_sys;

/// The size of the first segment of a DSA area: Postgres's DSM registry
/// keeps its index of named segments in one (`dsa_create`), created with
/// the cluster's first named segment.
#[cfg(any(feature = "pg17", feature = "pg18", test))]
const DSA_INITIAL_SEGMENT: u64 = 1024 * 1024;

/// The room `GetNamedDSMSegment` may take to create a named segment of
/// `size` bytes: the segment, the DSM registry's area if no one created it
/// first, and as much again to spare.
#[cfg(any(feature = "pg17", feature = "pg18", test))]
pub(crate) const fn named_segment_room(size: usize) -> u64 {
    size as u64 + 2 * DSA_INITIAL_SEGMENT
}

/// The bytes available where segments are created, reserve included, and
/// the store's size: `/dev/shm`'s for `posix` segments, `pg_dynshmem`'s
/// for `mmap` ones, unbounded for `sysv`; `None` if they cannot be read.
pub(crate) fn store_space() -> Option<(u64, u64)> {
    // SAFETY: reads a setting on the main thread.
    let kind = unsafe { pg_sys::dynamic_shared_memory_type } as u32;
    match kind {
        pg_sys::DSM_IMPL_POSIX => space(c"/dev/shm"),
        pg_sys::DSM_IMPL_MMAP => space(c"pg_dynshmem"),
        _ => Some((u64::MAX, u64::MAX)),
    }
}

/// Where segments are created, for a notice of a shortage there.
pub(crate) fn store_name() -> &'static str {
    // SAFETY: reads a setting on the main thread.
    let kind = unsafe { pg_sys::dynamic_shared_memory_type } as u32;
    if kind == pg_sys::DSM_IMPL_MMAP {
        "pg_dynshmem"
    } else {
        "/dev/shm"
    }
}

/// The bytes available to an unprivileged process in the file system of
/// `path`, and its size, if they can be read.
fn space(path: &CStr) -> Option<(u64, u64)> {
    let mut stats = MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `path` is a C string and `stats` room for the result.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: `statvfs` succeeded, so it filled `stats`.
    let stats = unsafe { stats.assume_init() };
    Some((
        blocks_bytes(stats.f_bavail, stats.f_frsize),
        blocks_bytes(stats.f_blocks, stats.f_frsize),
    ))
}

/// `blocks` of `size` bytes, in bytes.
// The fields are u64 on Linux and narrower on some other targets.
#[allow(clippy::useless_conversion)]
fn blocks_bytes(blocks: libc::fsblkcnt_t, size: libc::c_ulong) -> u64 {
    u64::from(blocks).saturating_mul(u64::from(size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_room_is_read_from_the_file_system_or_is_none() {
        let (available, size) = space(c"/").unwrap();
        assert!(size > 0 && available <= size);
        assert_eq!(space(c"/no/such/directory"), None);
    }

    #[test]
    fn a_named_segment_needs_room_for_the_dsm_registrys_area_too() {
        assert_eq!(named_segment_room(10), 10 + 2 * 1024 * 1024);
    }
}
