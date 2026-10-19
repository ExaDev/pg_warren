//! Moving a version's persisted vectors into its compaction (DESIGN.md
//! §5.2): each vector's rows gathered in the new dense-id order, so every
//! live node keeps its value and tombstoned rows are dropped.

use warren_core::compute::{AnyVector, ComputeError, ComputeMemory, Value, Vector};
use warren_core::graph::DenseId;

use super::{forget, read_vector, write_vector, Binding, VectorError, VectorStore};
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
        for (target, &old) in out.values_mut().chunks_exact_mut(width).zip(rows) {
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
/// budget: a compaction already holds two versions of the graph. Returns
/// the names moved, in order.
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
    for record in &records {
        let vector = read_vector(store, &memory, record)?;
        let moved = gather(&memory, &vector, rows)?;
        drop(vector);
        let binding = Binding::new(graph_id, to, &record.name, record.created_by);
        write_vector(store, &binding, &moved, chunk_size)?;
        forget(record.vector);
        store.delete(from, &record.name)?;
    }
    Ok(records.into_iter().map(|record| record.name).collect())
}

/// The NOTICE `compact` raises for the vectors it moved.
pub(crate) fn moved_notice(names: &[String], from: VersionId, to: VersionId) -> String {
    let names: Vec<String> = names.iter().map(|name| format!("\"{name}\"")).collect();
    format!("moved vectors {} from version {from} to version {to}", names.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::MemoryVectorStore;
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
