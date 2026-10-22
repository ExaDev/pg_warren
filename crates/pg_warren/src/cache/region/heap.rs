//! A region in the Rust allocator: a backend-local loaded version
//! (DESIGN.md §5.5, §5.6).

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ptr::NonNull;

use super::{RegionError, RegionMemory, ALIGNMENT};

/// Zeroed heap memory aligned to [`ALIGNMENT`], which it owns.
pub(crate) struct HeapMemory {
    start: NonNull<u8>,
    len: usize,
}

// SAFETY: `HeapMemory` owns its allocation, as a `Box<[u8]>` does, and
// hands out references to it only through `&self` and `&mut self`.
unsafe impl Send for HeapMemory {}
// SAFETY: as for `Send`; shared references only read.
unsafe impl Sync for HeapMemory {}

impl HeapMemory {
    /// `len` zeroed bytes, or `OutOfMemory` (53200) rather than an abort
    /// if the allocator cannot provide them. Zeroed memory from the
    /// allocator is mapped lazily, so only the pages written are touched.
    pub(crate) fn zeroed(len: usize) -> Result<Self, RegionError> {
        let layout = layout(len)?;
        // SAFETY: `layout` has a non-zero size (at least one byte).
        let start = unsafe { alloc_zeroed(layout) };
        let start = NonNull::new(start).ok_or(RegionError::OutOfMemory { bytes: len })?;
        Ok(Self { start, len })
    }

    /// The bytes, to fill.
    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `start` is a live allocation of at least `len` bytes,
        // all initialized (zeroed, then only written as bytes or through
        // `in_place::typed_mut`, which writes initialized fields), and
        // `&mut self` makes this the only reference to it.
        unsafe { std::slice::from_raw_parts_mut(self.start.as_ptr(), self.len) }
    }
}

// SAFETY: the allocation stays at `start`, initialized, until `drop`, and
// changes only through `&mut self`, never while a shared reference lives.
unsafe impl RegionMemory for HeapMemory {
    fn bytes(&self) -> &[u8] {
        // SAFETY: as for `bytes_mut`; `&self` keeps it unchanged.
        unsafe { std::slice::from_raw_parts(self.start.as_ptr(), self.len) }
    }
}

impl Drop for HeapMemory {
    fn drop(&mut self) {
        if let Ok(layout) = layout(self.len) {
            // SAFETY: `start` was allocated by `zeroed` with this layout and
            // is freed only here, once.
            unsafe { dealloc(self.start.as_ptr(), layout) };
        }
    }
}

/// The allocation of `len` bytes, at least one.
fn layout(len: usize) -> Result<Layout, RegionError> {
    Layout::from_size_align(len.max(1), ALIGNMENT).map_err(|_| RegionError::TooLarge)
}
