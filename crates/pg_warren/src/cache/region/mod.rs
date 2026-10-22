//! The region a loaded version lives in (DESIGN.md §5.6, §14 item 29): one
//! allocation holding every array of the version at a 64-byte-aligned
//! offset in native layout, after a header and table of contents
//! (`layout`). A backend-local version's region is heap memory (`heap`).
//! `warren-core` reads each array through a shared view of the region
//! (`SegmentBuffer::shared`), which keeps the region alive, so it is freed
//! when the last view of it drops. Typed slices are formed from the
//! region's bytes only here (`in_place`), after the alignment, bounds and
//! element width are checked.

mod error;
mod heap;
mod in_place;
mod layout;

use std::marker::PhantomData;
use std::mem::size_of;
use std::sync::Arc;

use warren_core::graph::{SegmentBuffer, SharedSegment};

use crate::catalog::VersionId;

pub(crate) use error::RegionError;
pub(crate) use heap::HeapMemory;
pub(crate) use in_place::{typed, typed_mut, write_pair, InPlace, Unpadded};
pub(crate) use layout::{Array, Entry, RegionLayout, ALIGNMENT};

/// Memory a region is read from.
///
/// # Safety
///
/// `bytes` returns the same initialized bytes, at the same address, on
/// every call, and they do not change while the memory lives.
pub(crate) unsafe trait RegionMemory: Send + Sync {
    /// The region's bytes.
    fn bytes(&self) -> &[u8];
}

/// A loaded version's region, read-only, with its checked layout.
pub(crate) struct Region {
    memory: Box<dyn RegionMemory>,
    layout: RegionLayout,
}

impl Region {
    /// The region of `version` in `memory`, after checking that it starts
    /// at [`ALIGNMENT`] and holds the layout its header describes: written
    /// for `version`, in this layout, every array aligned and inside it.
    pub(crate) fn open(
        memory: Box<dyn RegionMemory>,
        version: VersionId,
    ) -> Result<Arc<Self>, RegionError> {
        let address = memory.bytes().as_ptr() as usize;
        if !address.is_multiple_of(ALIGNMENT) {
            return Err(RegionError::Misaligned { address });
        }
        let layout = RegionLayout::read(memory.bytes(), version)?;
        Ok(Arc::new(Self { memory, layout }))
    }

    /// Its layout.
    pub(crate) fn layout(&self) -> &RegionLayout {
        &self.layout
    }

    /// Its bytes.
    pub(crate) fn bytes(&self) -> &[u8] {
        self.memory.bytes()
    }

    /// The bytes of `array`, if it holds it.
    pub(crate) fn array_bytes(&self, array: Array) -> Option<&[u8]> {
        let entry = self.layout.entry(array)?;
        self.bytes().get(entry.range())
    }

    /// A shared view of `array` as elements of `T`, if the region holds
    /// it, which keeps the region alive; an error if its elements are not
    /// `T`-wide.
    pub(crate) fn view<T: InPlace>(
        self: &Arc<Self>,
        array: Array,
    ) -> Result<Option<SegmentBuffer<T>>, RegionError> {
        let Some(entry) = self.layout.entry(array) else {
            return Ok(None);
        };
        if entry.width != size_of::<T>() {
            return Err(RegionError::ElementWidth { expected: entry.width, found: size_of::<T>() });
        }
        // Checks the alignment for `T` and the bounds, once.
        let found = self.bytes().len() as u64;
        let short = RegionError::Short { needed: entry.range().end as u64, found };
        let bytes = self.bytes().get(entry.range()).ok_or(short)?;
        typed::<T>(bytes)?;
        let view = View::<T> { region: Arc::clone(self), entry: *entry, elements: PhantomData };
        Ok(Some(SegmentBuffer::shared(Arc::new(view))))
    }
}

/// One array of a region as elements of `T`.
struct View<T> {
    region: Arc<Region>,
    entry: Entry,
    elements: PhantomData<fn() -> T>,
}

impl<T: InPlace> SharedSegment<T> for View<T> {
    fn elements(&self) -> &[T] {
        let start = self.region.bytes().as_ptr().wrapping_add(self.entry.offset);
        // SAFETY: `Region::view` checked, when it made this view, that the
        // entry's bytes lie inside the region and are aligned for `T` and
        // `elements` elements of `T` long (`in_place::typed`); the region's
        // memory keeps the same initialized bytes at the same address while
        // it lives (`RegionMemory`'s contract), which is at least as long
        // as `self.region`, and so as the slice borrowed from `self`; any
        // initialized bytes are a valid `T` (`InPlace`).
        unsafe { std::slice::from_raw_parts(start.cast::<T>(), self.entry.elements) }
    }
}

#[cfg(test)]
mod tests;
