//! A region in the Rust allocator: a backend-local loaded version
//! (DESIGN.md §5.5, §5.6).

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ptr::NonNull;

use super::{RegionError, RegionMemory, ALIGNMENT};

/// Zeroed heap memory aligned to [`ALIGNMENT`], which it owns.
///
/// It is allocated byte-aligned, [`ALIGNMENT`] bytes longer than asked,
/// and starts at the first aligned byte. On Unix the standard allocator
/// zeroes an allocation aligned to at most its minimum alignment with
/// `calloc`, which maps a large one from fresh zero pages untouched until
/// written; one aligned to 64 it would allocate aligned and then `memset`
/// whole, a second pass over every byte the loader then writes anyway.
pub(crate) struct HeapMemory {
    /// The allocation.
    allocation: NonNull<u8>,
    /// Where the memory starts in it: the first aligned byte.
    start: usize,
    len: usize,
}

// SAFETY: `HeapMemory` owns its allocation, as a `Box<[u8]>` does, and
// hands out references to it only through `&self` and `&mut self`.
unsafe impl Send for HeapMemory {}
// SAFETY: as for `Send`; shared references only read.
unsafe impl Sync for HeapMemory {}

impl HeapMemory {
    /// `len` zeroed bytes, or `OutOfMemory` (53200) rather than an abort
    /// if the allocator cannot provide them.
    pub(crate) fn zeroed(len: usize) -> Result<Self, RegionError> {
        let layout = layout(len)?;
        // SAFETY: `layout` has a non-zero size (more than `ALIGNMENT`).
        let allocation = unsafe { alloc_zeroed(layout) };
        let allocation = NonNull::new(allocation).ok_or(RegionError::OutOfMemory { bytes: len })?;
        // At most `ALIGNMENT - 1`, so `start + len` is inside the allocation.
        let start = allocation.as_ptr().align_offset(ALIGNMENT);
        Ok(Self { allocation, start, len })
    }

    /// The bytes, to fill.
    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `start..start + len` lies inside the live allocation
        // (`zeroed`), all initialized (zeroed, then only written as bytes
        // or through `in_place::typed_mut`, which writes initialized
        // fields), and `&mut self` makes this the only reference to it.
        unsafe {
            std::slice::from_raw_parts_mut(self.allocation.as_ptr().add(self.start), self.len)
        }
    }
}

// SAFETY: the allocation stays where it is, initialized, until `drop`,
// and changes only through `&mut self`, never while a shared reference
// lives.
unsafe impl RegionMemory for HeapMemory {
    fn bytes(&self) -> &[u8] {
        // SAFETY: as for `bytes_mut`; `&self` keeps it unchanged.
        unsafe { std::slice::from_raw_parts(self.allocation.as_ptr().add(self.start), self.len) }
    }
}

impl Drop for HeapMemory {
    fn drop(&mut self) {
        if let Ok(layout) = layout(self.len) {
            // SAFETY: `allocation` was allocated by `zeroed` with this
            // layout and is freed only here, once.
            unsafe { dealloc(self.allocation.as_ptr(), layout) };
        }
    }
}

/// The byte-aligned allocation that holds `len` bytes at an aligned start.
fn layout(len: usize) -> Result<Layout, RegionError> {
    let size = len.checked_add(ALIGNMENT).ok_or(RegionError::TooLarge)?;
    Layout::from_size_align(size, 1).map_err(|_| RegionError::TooLarge)
}
