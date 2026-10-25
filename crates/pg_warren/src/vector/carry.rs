//! Moving a version's persisted vectors into its compaction (DESIGN.md
//! §5.2): each vector's rows gathered in the new dense-id order, so every
//! live node keeps its value and tombstoned rows are dropped.

use warren_core::compute::{AnyVector, ComputeError, ComputeMemory, Value, Vector};
use warren_core::graph::DenseId;

use super::{forget, read_vector, write_vector, Binding, VectorError, VectorRecord, VectorStore};
use crate::activity;
use crate::catalog::VersionId;

/// `vector`'s rows `rows` (old dense ids), in order, as a new vector
/// charged to `memory`. A row past the end of `vector` is zero.
pub(crate) fn gather(
    memory: &ComputeMemory,
    vector: &AnyVector,
    rows: &[DenseId],
) -> Result<AnyVector, ComputeError> {
    fn gathered<T: Value>(
        memory: &ComputeMemory,
        vector: &Vector<T>,
        rows: &[DenseId],
    ) -> Result<Vector<T>, ComputeError> {
        let width = vector.width();
        let mut out = Vector::filled(memory, rows.len() as u64, width, T::default())?;
        for (target, &old) in out.values_mut()?.chunks_exact_mut(width).zip(rows) {
            let start = usize::try_from(old).unwrap_or(usize::MAX).saturating_mul(width);
            if let Some(source) = vector.values().get(start..start.saturating_add(width)) {
                target.copy_from_slice(source);
            }
        }
        Ok(out)
    }
    Ok(match vector {
        AnyVector::F32(vector) => gathered(memory, vector, rows)?.into(),
        AnyVector::F64(vector) => gathered(memory, vector, rows)?.into(),
        AnyVector::I64(vector) => gathered(memory, vector, rows)?.into(),
        AnyVector::Bool(vector) => gathered(memory, vector, rows)?.into(),
    })
}

/// Moves every persisted vector bound to version `from` of graph
/// `graph_id` to version `to`, under the same name and creator, with its
/// rows gathered as `rows` (the old dense id of each new one) says, and
/// deletes it from `from`. One vector is held at a time, in an unlimited
/// budget: a compaction already holds two versions of the graph. Each is
/// locked before it is read: one dropped since it was listed is not moved,
/// and one `reassign_owned` gave to another role since is moved under that
/// role. Returns the names moved, in order.
pub(crate) fn move_vectors(
    store: &mut impl VectorStore,
    graph_id: i32,
    from: VersionId,
    to: VersionId,
    rows: &[DenseId],
    chunk_size: usize,
) -> Result<Vec<String>, VectorError> {
    let memory = ComputeMemory::new(u64::MAX);
    let records = store.bound(from)?;
    activity::phase("moving vectors", Some(records.len() as u64));
    let mut moved_names = Vec::with_capacity(records.len());
    for (count, record) in (1..).zip(records) {
        // Locked before it is read: one dropped since it was listed is not
        // moved, and none is dropped or reassigned while it is.
        let Some(created_by) = store.lock(record.vector)? else {
            activity::done(count);
            continue;
        };
        let locked = VectorRecord { created_by, ..record };
        let vector = read_vector(store, &memory, &locked)?;
        let moved = gather(&memory, &vector, rows)?;
        drop(vector);
        let binding = Binding::new(graph_id, to, &locked.name, locked.created_by);
        write_vector(store, &binding, &moved, chunk_size)?;
        // By id: a vector of that name written to the source since it was
        // listed is not moved, so it must not be deleted either.
        if !store.delete(locked.checked())? {
            return Err(VectorError::WrittenConcurrently { name: locked.name, version: from });
        }
        forget(locked.vector);
        moved_names.push(locked.name);
        activity::done(count);
    }
    Ok(moved_names)
}

/// The NOTICE `compact` raises for the vectors it moved.
pub(crate) fn moved_notice(names: &[String], from: VersionId, to: VersionId) -> String {
    let names: Vec<String> = names.iter().map(|name| format!("\"{name}\"")).collect();
    format!("moved vectors {} from version {from} to version {to}", names.join(", "))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use super::*;
    use crate::segment::EachChunk;
    use crate::vector::{Checked, ChunkSource, MemoryVectorStore, VectorId};
    use pgrx::pg_sys;
    use warren_core::compute::Dtype;

    fn memory() -> ComputeMemory {
        ComputeMemory::new(1 << 20)
    }

    #[test]
    fn rows_are_gathered_in_the_new_order_for_every_dtype() {
        let rows = [0, 2, 3];
        let f64s: AnyVector =
            Vector::from_values(&memory(), 4, 2, vec![0.0, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5])
                .unwrap()
                .into();
        let expected: AnyVector =
            Vector::from_values(&memory(), 3, 2, vec![0.0, 0.5, 2.0, 2.5, 3.0, 3.5])
                .unwrap()
                .into();
        assert_eq!(gather(&memory(), &f64s, &rows).unwrap(), expected);
        let bools: AnyVector =
            Vector::from_values(&memory(), 4, 1, vec![true, false, false, true]).unwrap().into();
        let expected: AnyVector =
            Vector::from_values(&memory(), 3, 1, vec![true, false, true]).unwrap().into();
        assert_eq!(gather(&memory(), &bools, &rows).unwrap(), expected);
        let short: AnyVector = Vector::from_values(&memory(), 1, 1, vec![7i64]).unwrap().into();
        let padded: AnyVector = Vector::from_values(&memory(), 2, 1, vec![7i64, 0]).unwrap().into();
        assert_eq!(gather(&memory(), &short, &[0, 5]).unwrap(), padded);
    }

    #[test]
    fn moving_reports_the_vectors_moved_of_their_number() {
        let mut store = MemoryVectorStore::default();
        let ids: AnyVector = Vector::from_values(&memory(), 2, 1, vec![1i64, 2]).unwrap().into();
        for name in ["a", "b"] {
            let binding = Binding::new(1, 7, name, pg_sys::Oid::from(42));
            write_vector(&mut store, &binding, &ids, 64).unwrap();
        }
        let running = crate::activity::Running::opened("compact");
        move_vectors(&mut store, 1, 7, 9, &[1, 0], 64).unwrap();
        let frame = crate::activity::own().operation.frame;
        assert_eq!(frame.phase.as_str(), "moving vectors");
        assert_eq!((frame.done, frame.total), (Some(2), Some(2)));
        drop(running);
    }

    /// A store in which `change` happens right after a compaction lists the
    /// source's vectors: a concurrent transaction that committed between
    /// the listing and the read.
    struct ChangedSinceListed {
        store: MemoryVectorStore,
        change: fn(&mut MemoryVectorStore),
    }

    impl VectorStore for ChangedSinceListed {
        fn bound(&mut self, version: VersionId) -> Result<Vec<VectorRecord>, VectorError> {
            let listed = self.store.bound(version)?;
            (self.change)(&mut self.store);
            Ok(listed)
        }

        fn insert(
            &mut self,
            binding: &Binding<'_>,
            dtype: Dtype,
            width: usize,
            length: u64,
        ) -> Result<VectorId, VectorError> {
            self.store.insert(binding, dtype, width, length)
        }

        fn write_chunks(
            &mut self,
            vector: VectorId,
            name: &str,
            source: &mut ChunkSource<'_>,
        ) -> Result<(), VectorError> {
            self.store.write_chunks(vector, name, source)
        }

        fn read_chunks(
            &mut self,
            vector: VectorId,
            each: &mut EachChunk<'_, VectorError>,
        ) -> Result<(), VectorError> {
            self.store.read_chunks(vector, each)
        }

        fn lock(&mut self, vector: VectorId) -> Result<Option<pg_sys::Oid>, VectorError> {
            self.store.lock(vector)
        }

        fn delete(&mut self, vector: Checked) -> Result<bool, VectorError> {
            self.store.delete(vector)
        }

        fn existing(&mut self, vectors: &[VectorId]) -> Result<Vec<VectorId>, VectorError> {
            self.store.existing(vectors)
        }
    }

    /// Vectors "kept" and "changed" of creator 42 bound to version 7, in a
    /// store where `change` happens right after they are listed.
    fn listed_then(change: fn(&mut MemoryVectorStore)) -> ChangedSinceListed {
        let mut store = MemoryVectorStore::default();
        let ids: AnyVector = Vector::from_values(&memory(), 2, 1, vec![1i64, 2]).unwrap().into();
        for name in ["changed", "kept"] {
            let binding = Binding::new(1, 7, name, pg_sys::Oid::from(42));
            write_vector(&mut store, &binding, &ids, 64).unwrap();
        }
        ChangedSinceListed { store, change }
    }

    /// Vector "changed" of `store`.
    fn changed(store: &mut MemoryVectorStore) -> &mut VectorRecord {
        let found = store.rows.values_mut().find(|(_, record)| record.name == "changed");
        &mut found.unwrap().1
    }

    /// The names and creators of the vectors bound to `version`.
    fn bound(store: &mut MemoryVectorStore, version: VersionId) -> Vec<(String, pg_sys::Oid)> {
        let records = store.bound(version).unwrap();
        records.into_iter().map(|record| (record.name, record.created_by)).collect()
    }

    /// DESIGN.md §5.2: a vector dropped after `compact` listed the source's
    /// vectors is not moved (found by `test/concurrent_refresh.py`: it was
    /// read as a vector with no chunks, `data_corrupted`).
    #[test]
    fn a_vector_dropped_since_it_was_listed_is_not_moved() {
        let mut listing = listed_then(|store| {
            let vector = changed(store).checked();
            assert!(store.delete(vector).unwrap());
        });
        assert_eq!(move_vectors(&mut listing, 1, 7, 9, &[1, 0], 64).unwrap(), ["kept"]);
        assert_eq!(bound(&mut listing.store, 9), [("kept".into(), pg_sys::Oid::from(42))]);
        assert!(listing.store.bound(7).unwrap().is_empty());
    }

    /// DESIGN.md §5.2: a vector `reassign_owned` gave to another role after
    /// `compact` listed the source's vectors is still moved, under the
    /// role it was given to.
    #[test]
    fn a_vector_reassigned_since_it_was_listed_is_moved_under_its_new_creator() {
        let mut listing = listed_then(|store| changed(store).created_by = pg_sys::Oid::from(43));
        assert_eq!(move_vectors(&mut listing, 1, 7, 9, &[1, 0], 64).unwrap(), ["changed", "kept"]);
        assert_eq!(
            bound(&mut listing.store, 9),
            [("changed".into(), pg_sys::Oid::from(43)), ("kept".into(), pg_sys::Oid::from(42))]
        );
        assert!(listing.store.bound(7).unwrap().is_empty());
    }

    #[test]
    fn every_vector_of_a_version_is_moved_with_its_name_and_creator() {
        let mut store = MemoryVectorStore::default();
        let creator = pg_sys::Oid::from(42);
        let ids: AnyVector =
            Vector::from_values(&memory(), 3, 1, vec![10i64, 11, 12]).unwrap().into();
        write_vector(&mut store, &Binding::new(1, 7, "ids", creator), &ids, 64).unwrap();
        write_vector(&mut store, &Binding::new(1, 6, "other", creator), &ids, 64).unwrap();
        assert_eq!(move_vectors(&mut store, 1, 7, 9, &[2, 0], 64).unwrap(), ["ids"]);
        let carried = store.bound(9).unwrap();
        let [record] = carried.as_slice() else { panic!("{carried:?}") };
        assert_eq!(
            (record.name.as_str(), record.dtype, record.width, record.length, record.created_by),
            ("ids", Dtype::I64, 1, 2, creator)
        );
        let back = read_vector(&mut store, &memory(), record).unwrap();
        let expected: AnyVector =
            Vector::from_values(&memory(), 2, 1, vec![12i64, 10]).unwrap().into();
        assert_eq!(back, expected);
        assert!(store.bound(7).unwrap().is_empty(), "the source's vectors were moved");
        assert_eq!(store.bound(6).unwrap().len(), 1, "other versions keep theirs");
        assert_eq!(
            moved_notice(&["emb".into(), "id".into()], 3, 9),
            "moved vectors \"emb\", \"id\" from version 3 to version 9"
        );
    }
}
