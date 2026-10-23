use super::*;
use crate::graph::{
    BuildCharge, BuildMemory, FixedKeys, HeapSegment, IdMap, Permutation, Tombstones,
};
use proptest::prelude::*;
use std::collections::BTreeMap;

/// Text ids charged to `memory`.
fn ids_in(memory: &BuildMemory, ids: &[&[u8]]) -> TextIds {
    let mut text = TextIds::new(memory);
    for id in ids {
        text.push(id).unwrap();
    }
    text
}

/// The next build of `id_map` over `ids`.
fn rebuild(id_map: &IdMap<TextKeys>, ids: &[&[u8]]) -> IdMap<TextKeys> {
    id_map.rebuild_within(ids_in(&BuildMemory::unlimited(), ids)).unwrap().0
}

fn first_build(ids: &[&[u8]]) -> IdMap<TextKeys> {
    rebuild(&IdMap::new(), ids)
}

#[test]
fn the_first_build_assigns_dense_ids_in_byte_order() {
    let id_map = first_build(&[b"b", b"ab", b"", b"a", b"ab"]);
    assert_eq!((id_map.id_space(), id_map.node_count()), (4, 4));
    for (dense, id) in [b"".as_slice(), b"a", b"ab", b"b"].into_iter().enumerate() {
        assert_eq!(id_map.dense_id(id), Some(dense as DenseId));
        assert_eq!(id_map.external_id(dense as DenseId), Some(id));
    }
    assert_eq!(id_map.keys().id_bytes(), b"aabb");
    assert_eq!(id_map.keys().id_offsets(), [0, 0, 1, 3, 4]);
    assert_eq!(id_map.keys().id_order(), &IdOrder::U32(vec![0, 1, 2, 3].into()));
}

#[test]
fn an_empty_text_id_map_has_one_offset() {
    let id_map = IdMap::<TextKeys>::new();
    assert_eq!((id_map.id_space(), id_map.keys().id_offsets()), (0, [0].as_slice()));
    assert_eq!(id_map.dense_id(b""), None);
    assert_eq!(id_map.external_id(0), None);
    assert_eq!(first_build(&[]), id_map);
}

#[test]
fn a_rebuild_appends_fresh_ids_and_tombstones_the_missing() {
    let v1 = first_build(&[b"m", b"c", b"x"]);
    let v2 = rebuild(&v1, &[b"x", b"a", b"c", b"zz"]);
    // c → 0, m → 1 (tombstoned), x → 2, a → 3, zz → 4.
    assert_eq!(v2.dense_id(b"a"), Some(3));
    assert_eq!(v2.dense_id(b"zz"), Some(4));
    assert_eq!(v2.dense_id(b"m"), None);
    assert_eq!(v2.external_id(1), Some(b"m".as_slice()));
    assert!(v2.tombstones().contains(1));
    assert_eq!(v2.keys().id_bytes(), b"cmxazz");
    assert_eq!(v2.keys().id_order(), &IdOrder::U32(vec![3, 0, 1, 2, 4].into()));
    let v3 = rebuild(&v2, &[b"m", b"c"]);
    assert_eq!(v3.dense_id(b"m"), Some(1), "a reappearing id gets its dense id back");
    assert_eq!(v3.id_space(), 5);
}

#[test]
fn a_text_id_map_holds_its_three_arrays_and_its_tombstones() {
    let id_map = first_build(&[b"abc", b"de"]);
    let keys = id_map.keys().bytes();
    assert_eq!(keys, 5 + 3 * 8 + 2 * 4);
    assert_eq!(id_map.bytes(), keys + id_map.tombstones().bytes());
}

/// A rebuild into its base extends the base's bytes and offsets in place
/// (DESIGN.md §5.7): at its peak it holds the base, the arena, the fresh
/// ids and the new order and tombstones, about 2·N·L + 28·N bytes, and
/// never a second copy of the base's bytes.
#[test]
fn a_rebuild_into_its_base_extends_it_in_place() {
    let nodes = 2_000u32;
    let id = |node: u32| format!("https://example.org/node/{node:08}").into_bytes();
    let ids: Vec<Vec<u8>> = (0..nodes).map(id).collect();
    let base = first_build(&ids.iter().map(Vec::as_slice).collect::<Vec<_>>());
    let length = ids[0].len() as u64;
    let memory = BuildMemory::unlimited();
    let base_bytes = base.bytes();
    let charge = memory.charge(base_bytes).unwrap();
    // 100 kept, 1,900 tombstoned and 100 fresh.
    let next: Vec<Vec<u8>> = (0..100).chain(nodes..nodes + 100).map(id).collect();
    let arena = ids_in(&memory, &next.iter().map(Vec::as_slice).collect::<Vec<_>>());
    let held = memory.in_use() - base_bytes;
    let (rebuilt, charge) = base.rebuild_into(charge, arena).unwrap();
    assert_eq!(charge.bytes(), rebuilt.bytes());
    assert_eq!(rebuilt.dense_id(&id(nodes)), Some(u64::from(nodes)));
    let (n, fresh) = (u64::from(nodes) + 100, 100);
    let design = held + n * length + 28 * n + Tombstones::bytes_for(n);
    assert!(memory.peak() <= design, "{} > {design}", memory.peak());
    let exact = base_bytes + held + fresh * (length + 8) + 4 * n + Tombstones::bytes_for(n);
    assert!(memory.peak() <= exact, "{} > {exact}", memory.peak());
}

/// A rebuild from a borrowed base copies it, charged.
#[test]
fn a_rebuild_charges_what_it_holds() {
    let base = first_build(&[b"a", b"b", b"c"]);
    let memory = BuildMemory::unlimited();
    let (rebuilt, charge) = base.rebuild_within(ids_in(&memory, &[b"b", b"d"])).unwrap();
    assert_eq!(charge.bytes(), rebuilt.bytes());
    drop(charge);
    assert_eq!(memory.in_use(), 0);
    assert!(matches!(
        base.rebuild_within(ids_in(&BuildMemory::new(40), &[b"b"])),
        Err(GraphError::BuildMemoryLimit { .. })
    ));
}

#[test]
fn text_ids_are_charged_as_they_arrive() {
    let memory = BuildMemory::new(1 << 20);
    let ids = ids_in(&memory, &[b"abc", b"", b"abc"]);
    assert_eq!(ids.len(), 3);
    assert!(memory.in_use() >= 3 + 3 * 16);
    drop(ids);
    assert_eq!(memory.in_use(), 0);
    let mut ids = TextIds::new(&BuildMemory::new(8));
    assert!(matches!(ids.push(b"too long for it"), Err(GraphError::BuildMemoryLimit { .. })));
}

/// The locality order of the path 0 - 2 - 1 (`new = [2, 0, 1]`).
fn path_permutation() -> Permutation {
    let relation = crate::graph::RelationDictionary::new().intern("").unwrap();
    let mut edges = crate::graph::EdgeList::for_id_space(3, false);
    edges.push(relation, 0, 2, 1.0).unwrap();
    edges.push(relation, 2, 1, 1.0).unwrap();
    Permutation::locality(&edges, 3, &BuildMemory::unlimited()).unwrap()
}

#[test]
fn renumbering_moves_bytes_and_offsets_and_maps_the_order() {
    // a → 0, bb → 1 (tombstoned), ccc → 2.
    let mut id_map = rebuild(&first_build(&[b"a", b"bb", b"ccc"]), &[b"a", b"ccc"]);
    let memory = BuildMemory::unlimited();
    id_map.renumber(&path_permutation(), &memory).unwrap();
    assert_eq!(id_map.keys().id_bytes(), b"bbccca");
    assert_eq!(id_map.keys().id_offsets(), [0, 2, 5, 6]);
    assert_eq!(id_map.keys().id_order(), &IdOrder::U32(vec![2, 0, 1].into()));
    assert_eq!(id_map.dense_id(b"a"), Some(2));
    assert_eq!(id_map.dense_id(b"bb"), None);
    assert!(id_map.tombstones().contains(0));
    assert!(memory.peak() >= 6 + 4 * 8, "the new arrays are charged");
    assert_eq!(memory.in_use(), 0);

    let mut refused = first_build(&[b"a", b"bb", b"ccc"]);
    let before = refused.clone();
    assert!(matches!(
        refused.renumber(&path_permutation(), &BuildMemory::new(8)),
        Err(GraphError::BuildMemoryLimit { .. })
    ));
    assert_eq!(refused, before, "nothing is renumbered");
}

/// The three arrays as shared views of heap segments, through
/// `from_text_segments`, which checks them.
fn shared(id_map: &IdMap<TextKeys>) -> IdMap<TextKeys> {
    let view = id_map.shared_views();
    let TextKeys { bytes, offsets, order } = view.keys.clone();
    IdMap::from_text_segments(bytes, offsets, order, view.tombstones().clone()).unwrap()
}

#[test]
fn shared_views_read_as_the_owned_id_map_and_renumber_a_copy() {
    let owned = rebuild(&first_build(&[b"a", b"bb", b"ccc"]), &[b"a", b"ccc"]);
    let mut view = shared(&owned);
    assert_eq!(view, owned);
    assert_eq!(view.bytes(), owned.bytes());
    let mut expected = owned.clone();
    let memory = BuildMemory::unlimited();
    view.renumber(&path_permutation(), &memory).unwrap();
    expected.renumber(&path_permutation(), &memory).unwrap();
    assert_eq!(view, expected);
    let (rebuilt, _) =
        shared(&owned).rebuild_into(BuildCharge::none(&memory), ids_in(&memory, &[b"d"])).unwrap();
    assert_eq!(rebuilt, rebuild(&owned, &[b"d"]));
}

#[test]
fn shared_views_of_a_text_id_map_are_validated() {
    let bytes = |bytes: &[u8]| SegmentBuffer::shared(HeapSegment::of(bytes));
    let offsets = |offsets: &[u64]| SegmentBuffer::shared(HeapSegment::of(offsets));
    let order = |order: &[u32]| IdOrder::U32(SegmentBuffer::shared(HeapSegment::of(order)));
    let check = |b, o, d, id_space| {
        IdMap::from_text_segments(b, o, d, Tombstones::new(id_space).unwrap()).map(|_| ())
    };
    assert_eq!(check(bytes(b"ba"), offsets(&[0, 1, 2]), order(&[1, 0]), 2), Ok(()));
    assert_eq!(
        check(bytes(b"ba"), offsets(&[0, 1, 2]), order(&[0, 1]), 2),
        Err(GraphError::IdMapNotSorted)
    );
    assert_eq!(
        check(bytes(b"aa"), offsets(&[0, 1, 2]), order(&[0, 1]), 2),
        Err(GraphError::IdMapNotSorted),
        "a duplicated id"
    );
    assert_eq!(
        check(bytes(b"ba"), offsets(&[0, 1, 2]), order(&[1, 1]), 2),
        Err(GraphError::IdMapNotPermutation)
    );
    assert_eq!(
        check(bytes(b"ba"), offsets(&[0, 1, 2]), order(&[1, 2]), 2),
        Err(GraphError::IdMapNotPermutation)
    );
    assert_eq!(
        check(bytes(b"ba"), offsets(&[0, 1, 2]), order(&[1]), 2),
        Err(GraphError::SegmentLength { expected: 2, found: 1 })
    );
    assert_eq!(
        check(bytes(b"ba"), offsets(&[1, 1, 2]), order(&[1, 0]), 2),
        Err(GraphError::IdOffsetsStart)
    );
    assert_eq!(check(bytes(b"ba"), offsets(&[]), order(&[]), 0), Err(GraphError::IdOffsetsStart));
    assert_eq!(
        check(bytes(b"ba"), offsets(&[0, 2, 1]), order(&[1, 0]), 2),
        Err(GraphError::IdOffsetsNotMonotone { dense: 1 })
    );
    assert_eq!(
        check(bytes(b"ba"), offsets(&[0, 1, 3]), order(&[1, 0]), 2),
        Err(GraphError::IdOffsetsEnd { expected: 2, found: 3 })
    );
    assert_eq!(
        check(
            bytes(b"ba"),
            offsets(&[0, 1, 2]),
            IdOrder::U64(SegmentBuffer::shared(HeapSegment::of(&[1, 0]))),
            2
        ),
        Err(GraphError::TargetWidthMismatch)
    );
    assert_eq!(
        check(bytes(b"ba"), offsets(&[0, 1, 2]), order(&[1, 0]), 3),
        Err(GraphError::TombstonesLength { expected: 2, found: 3 })
    );
}

#[test]
fn the_hash_index_maps_live_ids_and_confirms_their_bytes() {
    let id_map = rebuild(&first_build(&[b"a", b"b", b"c"]), &[b"a", b"c", b"d"]);
    let memory = BuildMemory::unlimited();
    let index = TextIndex::new(&id_map, &memory).unwrap();
    assert_eq!(memory.in_use(), index.bytes());
    assert_eq!(index.bytes(), 2 * 3 * 4, "two u32 slots a live node");
    assert_eq!(index.dense_id(b"a"), Some(0));
    assert_eq!(index.dense_id(b"d"), Some(3));
    assert_eq!(index.dense_id(b"b"), None, "tombstoned");
    assert_eq!(index.dense_id(b"e"), None);
    drop(index);
    assert_eq!(memory.in_use(), 0);
    assert!(matches!(
        TextIndex::new(&id_map, &BuildMemory::new(8)),
        Err(GraphError::BuildMemoryLimit { .. })
    ));
    let empty = IdMap::<TextKeys>::new();
    assert_eq!(TextIndex::new(&empty, &memory).unwrap().dense_id(b""), None);
}

fn text_builds() -> impl Strategy<Value = Vec<Vec<Vec<u8>>>> {
    let id = prop::collection::vec(prop::sample::select(b"ab\x00\xff".to_vec()), 0..4);
    prop::collection::vec(prop::collection::vec(id, 0..30), 1..6)
}

proptest! {
    #[test]
    fn dense_ids_are_stable_across_a_sequence_of_text_builds(builds in text_builds()) {
        let mut id_map = IdMap::<TextKeys>::new();
        let mut ever: BTreeMap<Vec<u8>, DenseId> = BTreeMap::new();
        for (build, ids) in builds.iter().enumerate() {
            let live: std::collections::BTreeSet<&[u8]> = ids.iter().map(Vec::as_slice).collect();
            let previous_id_space = id_map.id_space();
            let refs: Vec<&[u8]> = ids.iter().map(Vec::as_slice).collect();
            id_map = match build % 2 {
                0 => rebuild(&id_map, &refs),
                _ => {
                    let memory = BuildMemory::unlimited();
                    let charge = memory.charge(id_map.bytes()).unwrap();
                    id_map.rebuild_into(charge, ids_in(&memory, &refs)).unwrap().0
                }
            };
            prop_assert_eq!(id_map.node_count(), live.len() as u64);
            let mut fresh = 0;
            for id in &live {
                match ever.get(*id) {
                    Some(&kept) => prop_assert_eq!(id_map.dense_id(id), Some(kept)),
                    None => {
                        prop_assert_eq!(id_map.dense_id(id), Some(previous_id_space + fresh));
                        ever.insert(id.to_vec(), previous_id_space + fresh);
                        fresh += 1;
                    }
                }
            }
            prop_assert_eq!(id_map.id_space(), ever.len() as u64);
            let index = TextIndex::new(&id_map, &BuildMemory::unlimited()).unwrap();
            for (id, &dense) in &ever {
                prop_assert_eq!(id_map.external_id(dense), Some(id.as_slice()));
                prop_assert_eq!(id_map.tombstones().contains(dense), !live.contains(id.as_slice()));
                prop_assert_eq!(index.dense_id(id), id_map.dense_id(id));
            }
            prop_assert_eq!(index.dense_id(b"\x01"), None);
            prop_assert_eq!(&shared(&id_map), &id_map);
        }
    }

    /// Under the order-preserving bijection from each id to its rank, a
    /// text id map is an `int8` id map, rebuilt and renumbered alike.
    #[test]
    fn a_text_id_map_is_an_int_id_map_of_ranks(builds in text_builds()) {
        let all: std::collections::BTreeSet<&[u8]> = builds.iter().flatten().map(Vec::as_slice).collect();
        let ranks: Vec<&[u8]> = all.into_iter().collect();
        let rank = |id: &[u8]| ranks.binary_search(&id).unwrap() as i64;
        let mut text = IdMap::<TextKeys>::new();
        let mut int = IdMap::<FixedKeys<i64>>::new();
        for ids in &builds {
            text = rebuild(&text, &ids.iter().map(Vec::as_slice).collect::<Vec<_>>());
            int = int.rebuild(ids.iter().map(|id| rank(id)).collect()).unwrap();
            prop_assert_eq!(text.tombstones(), int.tombstones());
            for dense in 0..int.id_space() {
                let id = text.external_id(dense).unwrap();
                prop_assert_eq!(Some(&rank(id)), int.external_id(dense));
            }
            let in_order: Vec<_> = text.keys().in_order().map(|(id, dense)| (rank(id), dense)).collect();
            let expected: Vec<_> = int.keys().in_order().map(|(&id, dense)| (id, dense)).collect();
            prop_assert_eq!(in_order, expected);
        }
    }
}
