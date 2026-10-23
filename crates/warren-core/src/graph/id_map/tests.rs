use super::*;
use crate::graph::{BuildBuffer, HeapSegment, SegmentBuffer};
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::mem::{offset_of, size_of};

fn first_build(externals: Vec<i64>) -> IdMap<FixedKeys<i64>> {
    IdMap::new().rebuild(externals).unwrap()
}

#[test]
fn the_first_build_assigns_dense_ids_in_external_id_sort_order() {
    let id_map = first_build(vec![30, 10, 20, 10]);
    assert_eq!(id_map.id_space(), 3);
    assert_eq!(id_map.node_count(), 3);
    assert_eq!(id_map.dense_id(&10), Some(0));
    assert_eq!(id_map.dense_id(&20), Some(1));
    assert_eq!(id_map.dense_id(&30), Some(2));
    assert_eq!(id_map.external_id(2), Some(&30));
}

#[test]
fn an_id_map_holds_both_directions_and_its_tombstones() {
    let id_map = first_build(vec![30, 10, 20]);
    let pair = size_of::<IdPair<i64>>() as u64;
    assert_eq!(id_map.bytes(), 3 * pair + 3 * 8 + id_map.tombstones().bytes());
    assert_eq!(IdMap::<FixedKeys<i64>>::new().bytes(), 0);
}

/// A rebuild reserves exactly what the result holds: with a stable
/// node set, its peak is the externals and the result, not a second
/// copy of the base and a fresh-id buffer for every external.
#[test]
fn a_rebuild_charges_no_more_than_its_externals_and_its_result() {
    let nodes = 10_000i64;
    let base = first_build((0..nodes).collect());
    for externals in [(0..nodes).collect::<Vec<_>>(), (nodes / 2..nodes * 3 / 2).collect()] {
        let memory = BuildMemory::unlimited();
        let externals = BuildBuffer::from_vec(&memory, externals);
        let held = externals.bytes();
        let (rebuilt, charge) = base.rebuild_within(externals).unwrap();
        assert_eq!(charge.bytes(), rebuilt.bytes());
        assert!(
            memory.peak() <= held + rebuilt.bytes(),
            "{} > {held} + {}",
            memory.peak(),
            rebuilt.bytes()
        );
    }
}

/// Consuming the base gives the rebuild's id map, and the base's charge
/// becomes exactly the new id map's.
#[test]
fn a_rebuild_into_the_base_takes_over_its_charge() {
    let base = first_build(vec![10, 20, 30]);
    let memory = BuildMemory::unlimited();
    let ids = |ids: Vec<i64>| BuildBuffer::from_vec(&memory, ids);
    let (expected, _) = base.rebuild_within(ids(vec![40, 30, 10])).unwrap();
    let base_charge = memory.charge(base.bytes()).unwrap();
    let (rebuilt, charge) = base.rebuild_into(base_charge, ids(vec![40, 30, 10])).unwrap();
    assert_eq!(rebuilt, expected);
    assert_eq!(charge.bytes(), rebuilt.bytes());
    drop(charge);
    assert_eq!(memory.in_use(), 0);
}

/// The locality order of the path 0 - 2 - 1 (`new = [2, 0, 1]`), over
/// an id space of 3.
fn path_permutation() -> Permutation {
    let relation = crate::graph::RelationDictionary::new().intern("").unwrap();
    let mut edges = crate::graph::EdgeList::for_id_space(3, false);
    edges.push(relation, 0, 2, 1.0).unwrap();
    edges.push(relation, 2, 1, 1.0).unwrap();
    Permutation::locality(&edges, 3, &BuildMemory::unlimited()).unwrap()
}

#[test]
fn renumbering_moves_both_directions_and_the_tombstones() {
    // 10 → 0, 20 → 1 (tombstoned), 30 → 2.
    let mut id_map = first_build(vec![10, 20, 30]).rebuild(vec![10, 30]).unwrap();
    id_map.renumber(&path_permutation(), &BuildMemory::unlimited()).unwrap();
    assert_eq!(id_map.dense_id(&10), Some(2));
    assert_eq!(id_map.dense_id(&30), Some(1));
    assert_eq!(id_map.dense_id(&20), None);
    assert_eq!(
        (0..3).map(|dense| *id_map.external_id(dense).unwrap()).collect::<Vec<_>>(),
        [20, 30, 10]
    );
    assert!(id_map.tombstones().contains(0));
    assert_eq!(id_map.tombstones().count(), 1);
    assert_eq!((id_map.id_space(), id_map.node_count()), (3, 2));
    assert!(id_map.pairs().windows(2).all(|pair| pair[0].external < pair[1].external));
}

/// The renumbered tombstones are a new bitmap, charged while it is
/// built beside the old one; without tombstones nothing is allocated.
#[test]
fn renumbering_charges_the_new_tombstones() {
    let tombstoned = first_build(vec![10, 20, 30]).rebuild(vec![10, 30]).unwrap();
    let none = BuildMemory::new(0);
    let mut id_map = tombstoned.clone();
    assert!(matches!(
        id_map.renumber(&path_permutation(), &none),
        Err(GraphError::BuildMemoryLimit { .. })
    ));
    assert_eq!(id_map, tombstoned, "nothing is renumbered");
    let memory = BuildMemory::unlimited();
    id_map.renumber(&path_permutation(), &memory).unwrap();
    assert_eq!(memory.peak(), Tombstones::bytes_for(3));
    assert_eq!(memory.in_use(), 0);

    let mut live = first_build(vec![10, 20, 30]);
    live.renumber(&path_permutation(), &none).unwrap();
    assert_eq!(live.dense_id(&10), Some(2));
}

#[test]
fn renumbering_refuses_a_permutation_of_another_id_space() {
    let mut id_map = first_build(vec![10, 20]);
    assert_eq!(
        id_map.renumber(&path_permutation(), &BuildMemory::unlimited()),
        Err(GraphError::IdSpaceMismatch { expected: 2, found: 3 })
    );
    assert_eq!(id_map, first_build(vec![10, 20]));
}

/// The pairs and the inverse map as shared views of heap segments.
fn shared_parts<E: ExternalId + Send + Sync + 'static>(
    id_map: &IdMap<FixedKeys<E>>,
) -> (SegmentBuffer<IdPair<E>>, SegmentBuffer<E>) {
    let externals: Vec<E> =
        (0..id_map.id_space()).filter_map(|dense| id_map.external_id(dense)).copied().collect();
    let pairs = SegmentBuffer::shared(HeapSegment::of(id_map.pairs()));
    (pairs, SegmentBuffer::shared(HeapSegment::of(&externals)))
}

#[test]
fn an_id_map_of_shared_views_reads_as_the_owned_one() {
    let owned = first_build(vec![10, 20, 30]).rebuild(vec![10, 30, 5]).unwrap();
    let (pairs, by_dense) = shared_parts(&owned);
    let shared = IdMap::from_segments(pairs, by_dense, owned.tombstones().clone()).unwrap();
    assert_eq!(shared, owned);
    assert_eq!(shared.dense_id(&5), Some(3));
    assert_eq!(shared.dense_id(&20), None);
    assert_eq!(shared.external_id(1), Some(&20));
    assert_eq!((shared.id_space(), shared.node_count()), (4, 3));
    assert_eq!(shared.bytes(), owned.bytes());
}

#[test]
fn shared_views_of_an_id_map_are_validated() {
    let pair = |external, dense| IdPair { external, dense };
    let pairs = |pairs: &[IdPair<i64>]| SegmentBuffer::shared(HeapSegment::of(pairs));
    let externals = |externals: &[i64]| SegmentBuffer::shared(HeapSegment::of(externals));
    let check =
        |p, e, id_space| IdMap::from_segments(p, e, Tombstones::new(id_space).unwrap()).map(|_| ());
    let sorted = [pair(10, 1), pair(20, 0)];
    assert_eq!(check(pairs(&sorted), externals(&[20, 10]), 2), Ok(()));
    assert_eq!(
        check(pairs(&[pair(20, 0), pair(10, 1)]), externals(&[20, 10]), 2),
        Err(GraphError::IdMapNotSorted)
    );
    assert_eq!(
        check(pairs(&sorted), externals(&[10, 20]), 2),
        Err(GraphError::IdMapNotPermutation)
    );
    assert_eq!(
        check(pairs(&[pair(10, 2), pair(20, 0)]), externals(&[20, 10]), 2),
        Err(GraphError::IdMapNotPermutation)
    );
    assert_eq!(
        check(pairs(&sorted), externals(&[20]), 2),
        Err(GraphError::SegmentLength { expected: 2, found: 1 })
    );
    assert_eq!(
        check(pairs(&sorted), externals(&[20, 10]), 3),
        Err(GraphError::TombstonesLength { expected: 2, found: 3 })
    );
}

#[test]
fn renumbering_a_shared_id_map_copies_it() {
    let owned = first_build(vec![10, 20, 30]).rebuild(vec![10, 30]).unwrap();
    let (pairs, by_dense) = shared_parts(&owned);
    let region = owned.pairs().to_vec();
    let mut shared = IdMap::from_segments(pairs, by_dense, owned.tombstones().clone()).unwrap();
    let mut expected = owned.clone();
    let memory = BuildMemory::unlimited();
    shared.renumber(&path_permutation(), &memory).unwrap();
    expected.renumber(&path_permutation(), &memory).unwrap();
    assert_eq!(shared, expected);
    assert_eq!(owned.pairs(), region, "the shared pairs are unchanged");
}

#[test]
fn a_pair_has_a_fixed_layout() {
    assert_eq!((size_of::<IdPair<i32>>(), offset_of!(IdPair<i32>, dense)), (16, 8));
    assert_eq!((size_of::<IdPair<i64>>(), offset_of!(IdPair<i64>, dense)), (16, 8));
    assert_eq!((size_of::<IdPair<[u8; 16]>>(), offset_of!(IdPair<[u8; 16]>, dense)), (24, 16));
}

#[test]
fn unknown_ids_have_no_mapping() {
    let id_map = first_build(vec![1, 2]);
    assert_eq!(id_map.dense_id(&3), None);
    assert_eq!(id_map.external_id(2), None);
}

#[test]
fn a_rebuild_keeps_survivors_appends_new_ids_and_tombstones_the_missing() {
    let previous = first_build(vec![10, 20, 30]);
    let id_map = previous.rebuild(vec![40, 5, 30, 10]).unwrap();
    assert_eq!(id_map.dense_id(&10), Some(0));
    assert_eq!(id_map.dense_id(&30), Some(2));
    assert_eq!(id_map.dense_id(&5), Some(3));
    assert_eq!(id_map.dense_id(&40), Some(4));
    assert_eq!(id_map.dense_id(&20), None);
    assert_eq!(id_map.external_id(1), Some(&20));
    assert!(id_map.tombstones().contains(1));
    assert_eq!(id_map.id_space(), 5);
    assert_eq!(id_map.node_count(), 4);
}

#[test]
fn a_resurrected_id_gets_its_old_dense_id_back() {
    let id_map =
        first_build(vec![10, 20]).rebuild(vec![10]).unwrap().rebuild(vec![10, 20]).unwrap();
    assert_eq!(id_map.dense_id(&20), Some(1));
    assert!(!id_map.tombstones().contains(1));
    assert_eq!(id_map.id_space(), 2);
}

#[test]
fn uuids_sort_bytewise() {
    let low = [0u8; 16];
    let mut high = [0u8; 16];
    high[0] = 0xff;
    let id_map = IdMap::new().rebuild(vec![high, low]).unwrap();
    assert_eq!(id_map.dense_id(&low), Some(0));
    assert_eq!(id_map.dense_id(&high), Some(1));
}

fn builds() -> impl Strategy<Value = Vec<BTreeSet<i32>>> {
    prop::collection::vec(prop::collection::btree_set(0i32..40, 0..30), 1..8)
}

proptest! {
    #[test]
    fn dense_ids_are_stable_across_a_sequence_of_builds(builds in builds()) {
        let mut id_map = IdMap::<FixedKeys<i32>>::new();
        let mut ever: BTreeMap<i32, DenseId> = BTreeMap::new();
        for live in builds {
            let mut input: Vec<i32> = live.iter().copied().collect();
            input.extend(live.iter().take(3)); // duplicates
            input.reverse();
            let previous_id_space = id_map.id_space();
            id_map = id_map.rebuild(input).unwrap();

            prop_assert!(id_map.id_space() >= previous_id_space);
            prop_assert_eq!(id_map.node_count(), live.len() as u64);
            prop_assert_eq!(id_map.tombstones().count(), id_map.id_space() - live.len() as u64);
            let mut fresh = Vec::new();
            for external in &live {
                let dense = id_map.dense_id(external);
                prop_assert!(dense.is_some());
                match ever.get(external) {
                    Some(&kept) => prop_assert_eq!(dense, Some(kept)),
                    None => fresh.push((*external, dense.unwrap_or_default())),
                }
            }
            // New ids are appended at the high-water mark in sort order.
            for (offset, (external, dense)) in fresh.iter().enumerate() {
                prop_assert_eq!(*dense, previous_id_space + offset as u64);
                ever.insert(*external, *dense);
            }
            prop_assert_eq!(id_map.id_space(), ever.len() as u64);
            for (external, dense) in &ever {
                prop_assert_eq!(id_map.external_id(*dense), Some(external));
                prop_assert_eq!(id_map.tombstones().contains(*dense), !live.contains(external));
            }
        }
    }
}
