//! The backend-local cache of loaded vectors, keyed by vector id
//! (DESIGN.md §5.5). A vector id names one immutable vector, so an entry
//! never goes stale; it only becomes unreachable when its row is deleted
//! (the name written again, or its version or graph dropped), and is then
//! pruned.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use warren_core::compute::{AnyVector, ComputeMemory};

use super::{read_vector, VectorError, VectorId, VectorRecord, VectorStore};

/// One cached vector.
struct Entry {
    graph_id: i32,
    vector: Arc<AnyVector>,
}

/// Loaded vectors by vector id. Callers hold `Arc` clones, so evicting a
/// vector a call is still reading is safe.
#[derive(Default)]
pub(crate) struct VectorCache {
    entries: HashMap<VectorId, Entry>,
}

impl VectorCache {
    /// Vector `vector`, if cached.
    pub(crate) fn get(&self, vector: VectorId) -> Option<Arc<AnyVector>> {
        self.entries.get(&vector).map(|entry| Arc::clone(&entry.vector))
    }

    /// Caches `vector` as `id`, of graph `graph_id`.
    pub(crate) fn insert(&mut self, graph_id: i32, id: VectorId, vector: Arc<AnyVector>) {
        self.entries.insert(id, Entry { graph_id, vector });
    }

    /// Drops vector `id`, if cached.
    pub(crate) fn remove(&mut self, id: VectorId) {
        self.entries.remove(&id);
    }

    /// The cached vector ids, ascending.
    pub(crate) fn vectors(&self) -> Vec<VectorId> {
        let mut vectors: Vec<_> = self.entries.keys().copied().collect();
        vectors.sort_unstable();
        vectors
    }

    /// Drops every cached vector not in `existing`.
    pub(crate) fn retain(&mut self, existing: &[VectorId]) {
        self.entries.retain(|vector, _| existing.contains(vector));
    }

    /// Drops every cached vector of graph `graph_id`; returns how many.
    pub(crate) fn evict_graph(&mut self, graph_id: i32) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, entry| entry.graph_id != graph_id);
        before - self.entries.len()
    }
}

thread_local! {
    /// This backend's loaded vectors.
    static CACHE: RefCell<VectorCache> = RefCell::new(VectorCache::default());
}

/// The vector `record` describes, of graph `graph_id`: from this backend's
/// cache, or read through `store` into a buffer charged to `memory` and
/// cached. A read first prunes cached vectors whose rows were deleted.
pub(crate) fn load(
    store: &mut impl VectorStore,
    memory: &ComputeMemory,
    graph_id: i32,
    record: &VectorRecord,
) -> Result<Arc<AnyVector>, VectorError> {
    if let Some(vector) = CACHE.with_borrow(|cache| cache.get(record.vector)) {
        return Ok(vector);
    }
    // Store calls run SPI, and may raise an ERROR or reach user code that
    // calls back into this cache, so they run outside any borrow of it.
    let cached = CACHE.with_borrow(VectorCache::vectors);
    let existing = store.existing(&cached)?;
    CACHE.with_borrow_mut(|cache| cache.retain(&existing));
    let vector = Arc::new(read_vector(store, memory, record)?);
    remember(graph_id, record.vector, Arc::clone(&vector));
    Ok(vector)
}

/// Caches `vector` as `id`, of graph `graph_id`: a vector just written.
pub(crate) fn remember(graph_id: i32, id: VectorId, vector: Arc<AnyVector>) {
    CACHE.with_borrow_mut(|cache| cache.insert(graph_id, id, vector));
}

/// Drops vector `id` from this backend's cache: its row was just deleted.
pub(crate) fn forget(id: VectorId) {
    CACHE.with_borrow_mut(|cache| cache.remove(id));
}

/// Drops this backend's cached vectors of graph `graph_id`; returns how
/// many there were.
pub(crate) fn evict_vectors(graph_id: i32) -> usize {
    CACHE.with_borrow_mut(|cache| cache.evict_graph(graph_id))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use super::*;
    use crate::vector::Binding;
    use crate::vector::{write_vector, MemoryVectorStore};
    use pgrx::pg_sys;
    use warren_core::compute::Vector;

    fn memory() -> ComputeMemory {
        ComputeMemory::new(1 << 20)
    }

    fn vector(value: f64) -> Arc<AnyVector> {
        Arc::new(Vector::filled(&memory(), 2, 1, value).unwrap().into())
    }

    #[test]
    fn a_cached_vector_is_shared_and_evicted_by_graph() {
        let mut cache = VectorCache::default();
        let held = vector(1.0);
        cache.insert(10, 1, Arc::clone(&held));
        cache.insert(10, 2, vector(2.0));
        cache.insert(11, 3, vector(3.0));
        assert!(Arc::ptr_eq(&cache.get(1).unwrap(), &held));
        assert_eq!(cache.evict_graph(10), 2);
        assert_eq!(cache.vectors(), [3]);
        assert_eq!(*held, *vector(1.0), "a caller holding a vector keeps it");
    }

    #[test]
    fn vectors_whose_rows_are_gone_are_pruned() {
        let mut cache = VectorCache::default();
        for id in [1, 2, 3] {
            cache.insert(10, id, vector(0.0));
        }
        cache.retain(&[2, 9]);
        assert_eq!(cache.vectors(), [2]);
        cache.remove(2);
        assert!(cache.get(2).is_none());
    }

    #[test]
    fn a_load_reads_once_then_hits_the_cache_and_prunes_replaced_vectors() {
        let mut store = MemoryVectorStore::default();
        let ones: AnyVector = Vector::filled(&memory(), 3, 1, 1i64).unwrap().into();
        let first =
            write_vector(&mut store, &Binding::new(5, 7, "x", pg_sys::Oid::INVALID), &ones, 64)
                .unwrap();
        let loaded = load(&mut store, &memory(), 5, &first).unwrap();
        store.chunks.clear();
        let again = load(&mut store, &memory(), 5, &first).unwrap();
        assert!(Arc::ptr_eq(&loaded, &again), "the second load is a cache hit");
        let binding = Binding::new(5, 7, "x", pg_sys::Oid::INVALID).replacing(Some(first.vector));
        let second = write_vector(&mut store, &binding, &ones, 64).unwrap();
        load(&mut store, &memory(), 5, &second).unwrap();
        let cached = CACHE.with_borrow(VectorCache::vectors);
        assert_eq!(cached, [second.vector], "the replaced vector was pruned");
        assert_eq!(evict_vectors(5), 1);
    }
}
