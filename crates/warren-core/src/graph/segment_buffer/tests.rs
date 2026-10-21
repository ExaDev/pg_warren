use std::sync::Arc;

use proptest::prelude::*;

use super::*;

#[test]
fn an_owned_buffer_reads_as_its_vector() {
    let buffer = SegmentBuffer::from(vec![3u64, 1, 2]);
    assert_eq!(buffer.as_slice(), &[3, 1, 2]);
    assert_eq!(buffer.len(), 3);
    assert_eq!(&buffer[1..], &[1, 2]);
}

#[test]
fn an_owned_buffer_holds_its_capacity() {
    let mut values = Vec::with_capacity(8);
    values.extend([1u32, 2, 3]);
    assert_eq!(SegmentBuffer::from(values).bytes(), 8 * 4);
}

#[test]
fn a_shared_view_reads_the_region_it_views() {
    let region = HeapSegment::of(&[10u64, 20, 30, 40]);
    let buffer = SegmentBuffer::shared(region);
    assert_eq!(buffer.as_slice(), &[10, 20, 30, 40]);
    assert_eq!(buffer.bytes(), 4 * 8, "a view counts the bytes it views");
}

#[test]
fn a_shared_view_outlives_its_creators_handle() {
    let region = HeapSegment::of(&[1.5f64, 2.5]);
    let buffer = SegmentBuffer::shared(Arc::clone(&region) as _);
    drop(region);
    let copy = buffer.clone();
    drop(buffer);
    assert_eq!(copy.as_slice(), &[1.5, 2.5]);
}

#[test]
fn owned_and_shared_buffers_of_the_same_elements_are_equal() {
    let owned = SegmentBuffer::from(vec![1i32, 2]);
    let shared = SegmentBuffer::shared(HeapSegment::of(&[1i32, 2]));
    assert_eq!(owned, shared);
    assert_eq!(format!("{owned:?}"), format!("{shared:?}"));
    assert_ne!(owned, SegmentBuffer::from(vec![1i32]));
}

#[test]
fn making_an_owned_buffer_mutable_changes_it_in_place() {
    let mut buffer = SegmentBuffer::from(vec![1u64, 2]);
    let before = buffer.as_slice().as_ptr();
    buffer.make_mut().unwrap()[0] = 7;
    assert_eq!(buffer.as_slice(), &[7, 2]);
    assert_eq!(buffer.as_slice().as_ptr(), before, "no copy");
}

#[test]
fn making_a_shared_view_mutable_copies_it_and_leaves_the_region_alone() {
    let region = HeapSegment::of(&[1u64, 2]);
    let mut buffer = SegmentBuffer::<u64>::shared(Arc::clone(&region) as _);
    assert_eq!(buffer.as_ptr(), region.elements().as_ptr());
    buffer.make_mut().unwrap().push(3);
    assert_ne!(buffer.as_ptr(), region.elements().as_ptr());
    assert_eq!(buffer.as_slice(), &[1, 2, 3]);
    assert_eq!(region.elements(), &[1, 2]);
}

proptest! {
    #[test]
    fn owned_and_shared_views_read_the_same_elements(values in prop::collection::vec(any::<u64>(), 0..200)) {
        let owned = SegmentBuffer::from(values.clone());
        let shared = SegmentBuffer::shared(HeapSegment::of(&values));
        prop_assert_eq!(owned.as_slice(), values.as_slice());
        prop_assert_eq!(shared.as_slice(), values.as_slice());
        prop_assert_eq!(&owned, &shared);
    }
}
