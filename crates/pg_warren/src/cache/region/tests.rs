use std::sync::Arc;

use warren_core::graph::IdPair;

use super::*;
use crate::segment::{CsrPart, CsrScope, Direction};

const OFFSETS: Array = Array::Csr(Direction::Forward, CsrPart::Offsets, CsrScope::Relation(0));

/// A region of version 7 holding offsets `[0, 2, 5, 9]` and pairs.
fn filled() -> HeapMemory {
    let arrays = [(OFFSETS, 8, 4), (Array::IdPairs, 16, 2)];
    let layout = RegionLayout::plan(7, &arrays).unwrap();
    let mut memory = HeapMemory::zeroed(layout.len()).unwrap();
    layout.write_header(memory.bytes_mut());
    let range = layout.entry(OFFSETS).unwrap().range();
    typed_mut::<u64>(&mut memory.bytes_mut()[range]).unwrap().copy_from_slice(&[0, 2, 5, 9]);
    let range = layout.entry(Array::IdPairs).unwrap().range();
    let second = &mut memory.bytes_mut()[range][16..];
    write_pair(second, IdPair { external: -3i32, dense: 1 }).unwrap();
    memory
}

#[test]
fn heap_memory_is_aligned_and_zeroed() {
    let memory = HeapMemory::zeroed(1000).unwrap();
    assert_eq!(memory.bytes().len(), 1000);
    assert_eq!(memory.bytes().as_ptr() as usize % ALIGNMENT, 0);
    assert!(memory.bytes().iter().all(|&byte| byte == 0));
}

/// This process's resident bytes.
#[cfg(target_os = "linux")]
fn resident() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: usize = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    // SAFETY: `sysconf` only reads a configuration value.
    pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize
}

/// The fill writes every array, so zeroing the region beforehand would be
/// a second pass over it: a large region's zeroed pages are mapped lazily
/// and stay untouched until written.
#[test]
#[cfg(target_os = "linux")]
fn zeroed_heap_memory_is_not_touched_until_written() {
    let bytes = 256 << 20;
    let before = resident();
    let memory = HeapMemory::zeroed(bytes).unwrap();
    let grown = resident().saturating_sub(before);
    assert!(grown < bytes / 4, "{grown} of {bytes} bytes resident");
    assert_eq!(memory.bytes().as_ptr() as usize % ALIGNMENT, 0);
    assert_eq!(memory.bytes()[bytes - 1], 0);
}

#[test]
fn memory_that_cannot_be_allocated_is_an_error_not_an_abort() {
    let bytes = isize::MAX as usize - 2 * ALIGNMENT;
    assert_eq!(HeapMemory::zeroed(bytes).err(), Some(RegionError::OutOfMemory { bytes }));
}

#[test]
fn a_region_views_its_arrays_in_place_for_as_long_as_a_view_lives() {
    let region = Region::open(Box::new(filled()), 7).unwrap();
    let offsets = region.view::<u64>(OFFSETS).unwrap().unwrap();
    let pairs = region.view::<IdPair<i32>>(Array::IdPairs).unwrap().unwrap();
    let start = region.layout().entry(OFFSETS).unwrap().offset;
    assert_eq!(offsets.as_ptr() as usize, region.bytes().as_ptr() as usize + start);
    assert_eq!(region.view::<f64>(Array::Tombstones).unwrap(), None);
    drop(region);
    assert_eq!(&*offsets, &[0, 2, 5, 9]);
    assert_eq!(pairs[1], IdPair { external: -3, dense: 1 });
    assert_eq!(offsets.bytes(), 32);
}

#[test]
fn a_view_of_another_width_is_refused() {
    let region = Region::open(Box::new(filled()), 7).unwrap();
    assert_eq!(
        region.view::<u32>(OFFSETS).err(),
        Some(RegionError::ElementWidth { expected: 8, found: 4 })
    );
}

/// Memory whose region starts `shift` bytes into a heap allocation.
struct Shifted(HeapMemory, usize);

// SAFETY: a suffix of heap memory, which is initialized and unchanged
// while it lives.
unsafe impl RegionMemory for Shifted {
    fn bytes(&self) -> &[u8] {
        &self.0.bytes()[self.1..]
    }
}

#[test]
fn a_misaligned_region_is_refused() {
    let layout = RegionLayout::plan(7, &[(OFFSETS, 8, 4)]).unwrap();
    let mut memory = HeapMemory::zeroed(layout.len() + 8).unwrap();
    layout.write_header(&mut memory.bytes_mut()[8..]);
    let address = memory.bytes().as_ptr() as usize + 8;
    assert_eq!(
        Region::open(Box::new(Shifted(memory, 8)), 7).err(),
        Some(RegionError::Misaligned { address })
    );
}

#[test]
fn typed_access_refuses_misaligned_or_ragged_bytes() {
    let mut memory = HeapMemory::zeroed(64).unwrap();
    assert!(typed_mut::<u64>(&mut memory.bytes_mut()[..16]).is_ok());
    let address = memory.bytes().as_ptr() as usize + 4;
    assert_eq!(
        typed_mut::<u64>(&mut memory.bytes_mut()[4..12]).err(),
        Some(RegionError::Misaligned { address })
    );
    assert_eq!(
        typed_mut::<u64>(&mut memory.bytes_mut()[..12]).err(),
        Some(RegionError::Ragged { bytes: 12, width: 8 })
    );
}

#[test]
fn a_pair_is_written_field_by_field_into_its_slot() {
    let mut memory = HeapMemory::zeroed(64).unwrap();
    memory.bytes_mut().fill(0xff);
    write_pair(&mut memory.bytes_mut()[..16], IdPair { external: 5i32, dense: 9 }).unwrap();
    let mut expected = [0xffu8; 16];
    expected[..4].copy_from_slice(&5i32.to_ne_bytes());
    expected[8..].copy_from_slice(&9u64.to_ne_bytes());
    assert_eq!(&memory.bytes()[..16], &expected);
    assert_eq!(
        write_pair(&mut memory.bytes_mut()[..24], IdPair { external: 5i32, dense: 9 }).err(),
        Some(RegionError::Ragged { bytes: 24, width: 16 })
    );
}

#[test]
fn a_region_is_shared_by_its_views() {
    let region = Region::open(Box::new(filled()), 7).unwrap();
    let views = (region.view::<u64>(OFFSETS).unwrap(), region.view::<u64>(OFFSETS).unwrap());
    assert_eq!(Arc::strong_count(&region), 3);
    drop(views);
    assert_eq!(Arc::strong_count(&region), 1);
}
