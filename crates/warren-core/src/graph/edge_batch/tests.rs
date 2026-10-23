use proptest::prelude::*;

use super::*;
use crate::graph::{EdgeIntake, EdgeList, FixedKeys, RelationDictionary, TextIds, TextKeys};
use crate::interrupts::Cancellation;

type Row<K> = (Option<K>, Option<K>, f64);

/// The text id map of `ids`, then a rebuild without `dropped`, which
/// tombstones it.
fn text_map(ids: &[&[u8]], dropped: &[u8]) -> IdMap<TextKeys> {
    let memory = BuildMemory::unlimited();
    let mut all = TextIds::new(&memory);
    let mut kept = TextIds::new(&memory);
    for id in ids {
        all.push(id).unwrap();
        if *id != dropped {
            kept.push(id).unwrap();
        }
    }
    let (first, _) = IdMap::new().rebuild_within(all).unwrap();
    first.rebuild_within(kept).unwrap().0
}

/// The edges of `rows` taken one at a time, and the dropped count.
fn row_by_row<K: IndexedKeys>(
    id_map: &IdMap<K>,
    directed: bool,
    rows: &[Row<&K::Key>],
) -> (Vec<(DenseId, DenseId)>, Vec<f64>, u64) {
    let relation = RelationDictionary::new().intern("r").unwrap();
    let mut intake = intake(id_map, directed);
    for &(source, target, weight) in rows {
        intake.push(relation, source, target, weight).unwrap();
    }
    finished(intake)
}

/// The edges of `rows` taken in batches of `size`, each mapped through
/// the index first if `map`, and the dropped count.
fn batched<K: IndexedKeys>(
    id_map: &IdMap<K>,
    directed: bool,
    rows: &[Row<&K::Key>],
    size: usize,
    map: bool,
) -> (Vec<(DenseId, DenseId)>, Vec<f64>, u64) {
    let memory = BuildMemory::unlimited();
    let relation = RelationDictionary::new().intern("r").unwrap();
    let index = K::index(id_map, &memory).unwrap();
    let mut intake = intake(id_map, directed);
    let mut batch = EdgeBatch::<K>::with_capacity(&memory, size).unwrap();
    for chunk in rows.chunks(size.max(1)) {
        for &(source, target, weight) in chunk {
            batch.push(relation, source, target, weight).unwrap();
        }
        if map {
            batch.map(&index, &Cancellation::new()).unwrap();
        }
        intake.take(&mut batch).unwrap();
        assert!(batch.is_empty());
    }
    finished(intake)
}

fn intake<K: IdKeys>(id_map: &IdMap<K>, directed: bool) -> EdgeIntake<'_, K> {
    match directed {
        true => EdgeIntake::directed(id_map, true),
        false => EdgeIntake::undirected(id_map, true),
    }
}

fn finished<K: IdKeys>(intake: EdgeIntake<'_, K>) -> (Vec<(DenseId, DenseId)>, Vec<f64>, u64) {
    let (list, dropped): (EdgeList, u64) = intake.finish();
    let edges = list.edges().map(|(_, source, target)| (source, target)).collect();
    (edges, list.weights().unwrap_or_default().to_vec(), dropped)
}

#[test]
fn a_mapped_batch_takes_the_edges_rows_would() {
    let id_map = text_map(&[b"a", b"b", b"c", b"gone"], b"gone");
    let rows: Vec<Row<&[u8]>> = vec![
        (Some(b"a"), Some(b"b"), 1.0),
        (Some(b"b"), Some(b"b"), 2.0),
        (None, Some(b"c"), 3.0),
        (Some(b"c"), Some(b"unknown"), 4.0),
        (Some(b"gone"), Some(b"a"), 5.0),
        (Some(b"c"), Some(b"a"), 6.0),
        (Some(b""), Some(b"a"), 7.0),
    ];
    for directed in [true, false] {
        let expected = row_by_row(&id_map, directed, &rows);
        assert_eq!(expected.2, 4);
        for size in [1, 2, 7] {
            assert_eq!(batched(&id_map, directed, &rows, size, true), expected);
            assert_eq!(batched(&id_map, directed, &rows, size, false), expected);
        }
    }
}

#[test]
fn fixed_ids_are_batched_as_text_ids_are() {
    let first = IdMap::<FixedKeys<i64>>::new().rebuild(vec![10, 20, 30]).unwrap();
    let id_map = first.rebuild(vec![10, 30]).unwrap();
    let rows: Vec<Row<&i64>> = vec![
        (Some(&10), Some(&30), 1.0),
        (Some(&20), Some(&30), 2.0),
        (Some(&30), None, 3.0),
        (Some(&30), Some(&10), 4.0),
    ];
    let expected = row_by_row(&id_map, true, &rows);
    assert_eq!(expected, (vec![(0, 2), (2, 0)], vec![1.0, 4.0], 2));
    assert_eq!(batched(&id_map, true, &rows, 3, true), expected);
    assert_eq!(batched(&id_map, true, &rows, 3, false), expected);
}

#[test]
fn a_cancelled_map_stops() {
    let id_map = text_map(&[b"a", b"b"], b"");
    let memory = BuildMemory::unlimited();
    let index = TextKeys::index(&id_map, &memory).unwrap();
    let relation = RelationDictionary::new().intern("r").unwrap();
    let mut batch = EdgeBatch::<TextKeys>::with_capacity(&memory, 0).unwrap();
    batch.push(relation, Some(b"a"), Some(b"b"), 1.0).unwrap();
    let cancellation = Cancellation::new();
    cancellation.cancel();
    assert_eq!(batch.map(&index, &cancellation), Err(Cancelled));
}

#[test]
fn a_batch_is_charged_to_its_memory() {
    let memory = BuildMemory::new(1 << 20);
    let relation = RelationDictionary::new().intern("r").unwrap();
    let mut batch = EdgeBatch::<TextKeys>::with_capacity(&memory, 0).unwrap();
    batch.push(relation, Some(&[7; 1000]), None, 1.0).unwrap();
    assert!(memory.in_use() >= 1000, "{}", memory.in_use());
    assert_eq!(batch.bytes(), memory.in_use(), "it says what it holds");
    batch.clear();
    assert_eq!(batch.bytes(), memory.in_use(), "and keeps it once cleared");
    let mut small = EdgeBatch::<TextKeys>::with_capacity(&BuildMemory::new(100), 0).unwrap();
    assert!(small.push(relation, Some(&[7; 1000]), None, 1.0).is_err());
}

#[test]
fn a_batch_has_room_for_its_rows_up_front() {
    let memory = BuildMemory::new(1 << 20);
    let relation = RelationDictionary::new().intern("r").unwrap();
    let mut batch = EdgeBatch::<FixedKeys<i64>>::with_capacity(&memory, 10).unwrap();
    let row = EdgeBatch::<FixedKeys<i64>>::ROW_BYTES;
    assert_eq!(memory.in_use(), 10 * row);
    for at in 0..10 {
        batch.push(relation, Some(&at), Some(&at), 1.0).unwrap();
    }
    assert_eq!(memory.in_use(), 10 * row, "its rows fit");
    let text = EdgeBatch::<TextKeys>::with_capacity(&memory, 10).unwrap();
    assert_eq!(
        text.bytes(),
        10 * EdgeBatch::<TextKeys>::ROW_BYTES,
        "a text id's bytes grow as they come"
    );
    let too_many = EdgeBatch::<FixedKeys<i64>>::with_capacity(&BuildMemory::new(row * 9), 10);
    assert!(matches!(too_many, Err(GraphError::BuildMemoryLimit { .. })));
}

#[test]
fn the_text_index_is_as_large_as_it_says_before_it_is_built() {
    let (a, b, c): (&[u8], &[u8], &[u8]) = (b"a", b"b", b"c");
    for ids in [&[][..], &[a][..], &[a, b, c][..]] {
        let id_map = text_map(ids, b"b");
        let index = TextIndex::new(&id_map, &BuildMemory::unlimited()).unwrap();
        assert_eq!(TextIndex::bytes_for(&id_map), index.bytes());
    }
}

proptest! {
    #[test]
    fn batches_of_any_size_take_what_rows_take(
        ids in prop::collection::vec(prop::collection::vec(0u8..4, 0..3), 0..12),
        rows in prop::collection::vec(
            (prop::option::of(prop::collection::vec(0u8..4, 0..3)),
             prop::option::of(prop::collection::vec(0u8..4, 0..3))),
            0..40,
        ),
        size in 1usize..9,
        directed: bool,
    ) {
        let ids: Vec<&[u8]> = ids.iter().map(Vec::as_slice).collect();
        let dropped = ids.first().copied().unwrap_or_default();
        let id_map = text_map(&ids, dropped);
        let rows: Vec<Row<&[u8]>> = rows
            .iter()
            .enumerate()
            .map(|(at, (s, t))| (s.as_deref(), t.as_deref(), at as f64))
            .collect();
        let expected = row_by_row(&id_map, directed, &rows);
        prop_assert_eq!(batched(&id_map, directed, &rows, size, true), expected.clone());
        prop_assert_eq!(batched(&id_map, directed, &rows, size, false), expected);
    }
}
