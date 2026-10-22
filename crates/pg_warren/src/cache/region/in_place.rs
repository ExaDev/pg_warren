//! Elements a region holds in place, and typed slices of its bytes.

use std::mem::{align_of, offset_of, size_of};

use warren_core::graph::IdPair;

use super::RegionError;
use crate::source::IdDatum;

/// An element a region holds in place.
///
/// # Safety
///
/// Every pattern of `size_of::<Self>()` initialized bytes is a valid
/// `Self`, and its alignment is at most [`ALIGNMENT`](super::ALIGNMENT).
pub(crate) unsafe trait InPlace: Copy + Send + Sync + 'static {}

/// An element without padding, so writing one writes every byte of it.
///
/// # Safety
///
/// `Self` has no padding bytes.
pub(crate) unsafe trait Unpadded: InPlace {}

// SAFETY: plain integers and floats: any bits are a value; alignment ≤ 8.
unsafe impl InPlace for u32 {}
// SAFETY: as for `u32`.
unsafe impl InPlace for u64 {}
// SAFETY: as for `u32`.
unsafe impl InPlace for f32 {}
// SAFETY: as for `u32`.
unsafe impl InPlace for f64 {}
// SAFETY: `IdDatum`'s contract: any bits are an external id, aligned ≤ 8.
unsafe impl<E: IdDatum> InPlace for E {}
// SAFETY: a `#[repr(C)]` pair of an external id and a `u64`, both
// `InPlace`; its padding, if any, is not part of its value.
unsafe impl<E: IdDatum> InPlace for IdPair<E> {}
// SAFETY: integers and floats have no padding.
unsafe impl Unpadded for u32 {}
// SAFETY: as for `u32`.
unsafe impl Unpadded for u64 {}
// SAFETY: as for `u32`.
unsafe impl Unpadded for f64 {}
// SAFETY: `IdDatum`'s contract: an external id has no padding.
unsafe impl<E: IdDatum> Unpadded for E {}

/// `bytes` as elements of `T`, which must be aligned for `T` and a whole
/// number of them.
pub(crate) fn typed<T: InPlace>(bytes: &[u8]) -> Result<&[T], RegionError> {
    let elements = check::<T>(bytes)?;
    // SAFETY: `check` found `bytes` aligned for `T` and `elements` whole
    // elements long; its bytes are initialized, and any initialized bytes
    // are a valid `T` (`InPlace`); the slice borrows `bytes`, so it lives
    // no longer than they do and nothing changes them meanwhile.
    Ok(unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<T>(), elements) })
}

/// `bytes` as elements of `T`, mutably, with the checks of [`typed`]. `T`
/// has no padding, so what is written leaves every byte initialized.
pub(crate) fn typed_mut<T: Unpadded>(bytes: &mut [u8]) -> Result<&mut [T], RegionError> {
    let elements = check::<T>(bytes)?;
    // SAFETY: as for `typed`; the exclusive borrow of `bytes` passes to
    // the slice, which is the only way to them while it lives.
    Ok(unsafe { std::slice::from_raw_parts_mut(bytes.as_mut_ptr().cast::<T>(), elements) })
}

/// Writes `pair` into `slot`, the bytes of one `IdPair<E>`, field by
/// field, so its padding stays initialized.
pub(crate) fn write_pair<E: IdDatum>(slot: &mut [u8], pair: IdPair<E>) -> Result<(), RegionError> {
    let (start, dense) = (offset_of!(IdPair<E>, external), offset_of!(IdPair<E>, dense));
    let end = slot.len();
    if end != size_of::<IdPair<E>>() {
        return Err(RegionError::Ragged { bytes: end, width: size_of::<IdPair<E>>() });
    }
    typed_mut::<E>(&mut slot[start..start + size_of::<E>()])?[0] = pair.external;
    typed_mut::<u64>(&mut slot[dense..dense + size_of::<u64>()])?[0] = pair.dense;
    Ok(())
}

/// The number of `T` in `bytes`, if they are aligned for `T` and whole.
fn check<T: InPlace>(bytes: &[u8]) -> Result<usize, RegionError> {
    let (address, width) = (bytes.as_ptr() as usize, size_of::<T>());
    if !address.is_multiple_of(align_of::<T>()) {
        return Err(RegionError::Misaligned { address });
    }
    if width == 0 || !bytes.len().is_multiple_of(width) {
        return Err(RegionError::Ragged { bytes: bytes.len(), width });
    }
    Ok(bytes.len() / width)
}
