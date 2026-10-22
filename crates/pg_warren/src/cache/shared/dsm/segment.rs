//! A shared version's or persisted vector's DSM segment (DESIGN.md §5.6):
//! a 64-byte prefix that names what it holds (its key and the registry's
//! format, checked on every attach), then its region (`cache::region`),
//! which starts 64-byte aligned since a segment's mapping is page aligned.
//!
//! | Bytes | Prefix field |
//! |---|---|
//! | 0..4 | magic `WRSV` |
//! | 4..8 | the registry's format (`u32`) |
//! | 8..12 | the database's oid (`u32`) |
//! | 12..16 | pg_warren's extension oid (`u32`) |
//! | 16..24 | the version id or vector id (`i64`) |
//! | 24..28 | its kind (`u32`: 1 a version, 2 a vector) |
//! | 28..64 | reserved, 0 |
//!
//! Also where segments are created, and how many bytes are free there.

use std::ffi::CStr;
use std::mem::MaybeUninit;
use std::ptr::NonNull;

use pgrx::pg_sys;

use super::placement::FORMAT;
use crate::cache::region::{RegionMemory, ALIGNMENT};
use crate::cache::shared::slot::{Handle, Key};

/// The bytes before a version's region in its segment.
pub(in crate::cache::shared) const PREFIX: usize = ALIGNMENT;
const MAGIC: [u8; 4] = *b"WRSV";

/// A mapped segment of this backend.
#[derive(Debug, Clone, Copy)]
pub(super) struct Segment(NonNull<pg_sys::dsm_segment>);

// SAFETY: a `Segment` is only an address; it is passed to Postgres, which
// alone dereferences it, and only on the main thread (DESIGN.md §6.9).
unsafe impl Send for Segment {}
// SAFETY: as for `Send`.
unsafe impl Sync for Segment {}

impl Segment {
    /// Its handle, by which other backends attach it.
    pub(super) fn handle(self) -> Handle {
        // SAFETY: a segment this backend maps.
        Handle(unsafe { pg_sys::dsm_segment_handle(self.0.as_ptr()) })
    }

    /// Its bytes: where they start, and how many.
    fn mapped(self) -> (*mut u8, usize) {
        // SAFETY: a segment this backend maps.
        unsafe {
            let address = pg_sys::dsm_segment_address(self.0.as_ptr()).cast::<u8>();
            (address, pg_sys::dsm_segment_map_length(self.0.as_ptr()))
        }
    }

    /// The region after its prefix, as memory a region is read from.
    pub(super) fn region(self) -> SegmentRegion {
        let (address, len) = self.mapped();
        SegmentRegion { start: address.wrapping_add(PREFIX), len: len.saturating_sub(PREFIX) }
    }

    /// Keeps the segment until it is unpinned, and this backend's mapping
    /// of it until it detaches it, whatever transactions end in between.
    pub(super) fn keep(self) {
        // SAFETY: a segment this backend created and has not pinned.
        unsafe {
            pg_sys::dsm_pin_segment(self.0.as_ptr());
            pg_sys::dsm_pin_mapping(self.0.as_ptr());
        }
    }

    /// Pins an evicted segment again, which this backend maps: it is kept
    /// until it is unpinned again.
    pub(super) fn repin(self) {
        // SAFETY: a segment this backend maps, unpinned when it was
        // evicted and pinned only here since (under the registry's lock,
        // with its slot revived only after).
        unsafe { pg_sys::dsm_pin_segment(self.0.as_ptr()) };
    }

    /// Detaches this backend's mapping, freeing the segment if it was the
    /// last and the segment is unpinned. Postgres reports a failure to
    /// unmap or destroy it at WARNING.
    pub(super) fn detach(self) {
        // SAFETY: a segment this backend maps, whose memory nothing reads
        // any more (the caller's contract: no view of its region is held).
        unsafe { pg_sys::dsm_detach(self.0.as_ptr()) };
    }
}

/// The region of a mapped segment.
#[derive(Debug, Clone, Copy)]
pub(super) struct SegmentRegion {
    start: *mut u8,
    len: usize,
}

// SAFETY: an address and length; the bytes they name are shared memory,
// read by the main thread and worker threads alike, and written only by
// the loader before the version is published.
unsafe impl Send for SegmentRegion {}
// SAFETY: as for `Send`.
unsafe impl Sync for SegmentRegion {}

// SAFETY: a segment's mapping stays where it is until this backend
// detaches it, which it does only once no view of the region is held (a
// loaded version built over it keeps its mapping in use, `Mapped::in_use`,
// and the backend's exit detaches nothing itself: Postgres does, after
// the exit callbacks, among them the compute pool's, which stops any
// kernel's threads still reading it after a FATAL); its bytes, written by
// the loader before it builds any view, never change after.
unsafe impl RegionMemory for SegmentRegion {
    fn bytes(&self) -> &[u8] {
        // SAFETY: `start..start + len` lies inside the mapping, as above.
        unsafe { std::slice::from_raw_parts(self.start, self.len) }
    }
}

impl SegmentRegion {
    /// The bytes, to fill while the version loads.
    ///
    /// # Safety
    ///
    /// Only the loader, before it builds any view of the region.
    pub(super) unsafe fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: inside the mapping; the caller's contract makes this the
        // only reference to it.
        unsafe { std::slice::from_raw_parts_mut(self.start, self.len) }
    }
}

/// Creates a segment of `bytes` (its prefix included) to load `key`'s
/// version into, its prefix written; `None` if Postgres has no DSM control
/// slot left, which loads the version privately rather than failing. The
/// transaction's resource owner detaches it, freeing it, unless it is
/// kept first.
pub(super) fn create(bytes: u64, key: Key) -> Option<Segment> {
    let size = usize::try_from(bytes).ok()?;
    if size < PREFIX {
        return None;
    }
    let flags = pg_sys::DSM_CREATE_NULL_IF_MAXSEGMENTS as i32;
    // SAFETY: on the main thread inside a transaction; a short /dev/shm is
    // an ERROR, which the caller checks for first (`store_room`).
    let segment = Segment(NonNull::new(unsafe { pg_sys::dsm_create(size, flags) })?);
    let (address, len) = segment.mapped();
    // SAFETY: the new mapping holds `len` bytes, no fewer than the prefix,
    // and only this backend knows it.
    let prefix = unsafe { std::slice::from_raw_parts_mut(address, len.min(PREFIX)) };
    prefix.copy_from_slice(&encode(key));
    Some(segment)
}

/// Attaches the segment `handle` names and checks that it holds `key`'s
/// version; its mapping lasts until this backend detaches it. `None` if it
/// no longer exists or holds something else.
pub(super) fn attach(handle: Handle, key: Key) -> Option<Segment> {
    // SAFETY: on the main thread inside a transaction; a handle that names
    // no segment gives NULL.
    let segment = Segment(NonNull::new(unsafe { pg_sys::dsm_attach(handle.0) })?);
    let (address, len) = segment.mapped();
    // SAFETY: the mapping holds `len` bytes, published read-only.
    let prefix = unsafe { std::slice::from_raw_parts(address, len.min(PREFIX)) };
    if prefix != encode(key) {
        segment.detach();
        return None;
    }
    // SAFETY: as in `keep`: a segment this backend maps.
    unsafe { pg_sys::dsm_pin_mapping(segment.0.as_ptr()) };
    Some(segment)
}

/// Unpins the segment `handle` names, evicted: it is freed when its last
/// mapping is detached.
pub(super) fn unpin(handle: Handle) {
    // SAFETY: the registry pinned it when it was published, and evicts a
    // slot, which unpins it, once.
    unsafe { pg_sys::dsm_unpin_segment(handle.0) };
}

/// The prefix of `key`'s version's segment.
fn encode(key: Key) -> [u8; PREFIX] {
    let mut prefix = [0; PREFIX];
    prefix[..4].copy_from_slice(&MAGIC);
    prefix[4..8].copy_from_slice(&FORMAT.to_le_bytes());
    prefix[8..12].copy_from_slice(&key.database.to_u32().to_le_bytes());
    prefix[12..16].copy_from_slice(&key.extension.to_u32().to_le_bytes());
    prefix[16..24].copy_from_slice(&key.id.to_le_bytes());
    prefix[24..28].copy_from_slice(&(key.kind as u32).to_le_bytes());
    prefix
}

/// The bytes free where new segments are created: in `/dev/shm` for
/// `posix` segments (Postgres reserves them with `posix_fallocate`, so a
/// short `/dev/shm` is an ERROR the loader must not reach), in
/// `pg_dynshmem` for `mmap` ones; unbounded for `sysv`. 0 if it cannot be
/// read, which loads privately.
pub(super) fn store_room() -> u64 {
    // SAFETY: reads a setting on the main thread.
    let kind = unsafe { pg_sys::dynamic_shared_memory_type } as u32;
    match kind {
        pg_sys::DSM_IMPL_POSIX => free(c"/dev/shm"),
        pg_sys::DSM_IMPL_MMAP => free(c"pg_dynshmem"),
        _ => u64::MAX,
    }
}

/// The bytes available to an unprivileged process in the file system of
/// `path`, or 0 if it cannot be read.
fn free(path: &CStr) -> u64 {
    let mut stats = MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `path` is a C string and `stats` room for the result.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return 0;
    }
    // SAFETY: `statvfs` succeeded, so it filled `stats`.
    let stats = unsafe { stats.assume_init() };
    blocks_bytes(stats.f_bavail, stats.f_frsize)
}

/// `blocks` of `size` bytes, in bytes.
// The fields are u64 on Linux and narrower on some other targets.
#[allow(clippy::useless_conversion)]
fn blocks_bytes(blocks: libc::fsblkcnt_t, size: libc::c_ulong) -> u64 {
    u64::from(blocks).saturating_mul(u64::from(size))
}

/// Where the shortage of a refused segment lies, for its notice.
pub(in crate::cache::shared) fn store_name() -> &'static str {
    // SAFETY: as in `store_room`.
    let kind = unsafe { pg_sys::dynamic_shared_memory_type } as u32;
    if kind == pg_sys::DSM_IMPL_MMAP {
        "pg_dynshmem"
    } else {
        "/dev/shm"
    }
}

#[cfg(test)]
mod tests {
    use pgrx::pg_sys::Oid;

    use super::*;

    #[test]
    fn a_prefix_names_the_key_and_the_registrys_format() {
        let key = Key::version(Oid::from(5), Oid::from(16_384), -2);
        let prefix = encode(key);
        assert_eq!(&prefix[..4], b"WRSV");
        assert_eq!(prefix[4..8], FORMAT.to_le_bytes());
        assert_eq!(prefix[8..12], 5u32.to_le_bytes());
        assert_eq!(prefix[12..16], 16_384u32.to_le_bytes());
        assert_eq!(prefix[16..24], (-2i64).to_le_bytes());
        assert_eq!(prefix[24..28], 1u32.to_le_bytes(), "a version's kind");
        assert!(prefix[28..].iter().all(|&byte| byte == 0));
        let other = Key { id: 3, ..key };
        assert_ne!(encode(other), prefix, "another version's prefix differs");
        let vector = Key::vector(key.database, key.extension, key.id);
        assert_eq!(encode(vector)[24..28], 2u32.to_le_bytes(), "a vector's kind");
        assert_eq!(encode(vector)[..24], prefix[..24]);
    }

    #[test]
    fn free_room_is_read_from_the_file_system_or_is_none() {
        assert!(free(c"/") > 0);
        assert_eq!(free(c"/no/such/directory"), 0);
    }
}
