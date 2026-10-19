//! The backend-local map of loaded versions, keyed by version id
//! (DESIGN.md §5.5). A version is immutable once built, so an entry never
//! goes stale; it only becomes unreachable when its version is dropped,
//! and is then pruned.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use warren_core::graph::ExternalId;

use super::LoadedVersion;
use crate::catalog::VersionId;

/// One cached version: an `Arc<LoadedVersion<E>>` for the graph's id type.
struct Entry {
    graph_id: i32,
    version: Arc<dyn Any + Send + Sync>,
}

/// Loaded versions by version id. Callers hold `Arc` clones, so evicting a
/// version a scan is still reading is safe: the scan keeps its copy.
#[derive(Default)]
pub(crate) struct VersionCache {
    entries: HashMap<VersionId, Entry>,
}

impl VersionCache {
    /// Version `version`, if it is cached with external id type `E`.
    pub(crate) fn get<E: ExternalId + Send + Sync + 'static>(
        &self,
        version: VersionId,
    ) -> Option<Arc<LoadedVersion<E>>> {
        let entry = self.entries.get(&version)?;
        Arc::clone(&entry.version).downcast().ok()
    }

    /// Caches `loaded` as `version` of graph `graph_id`, replacing any
    /// entry for it, and returns it shared.
    pub(crate) fn insert<E: ExternalId + Send + Sync + 'static>(
        &mut self,
        graph_id: i32,
        version: VersionId,
        loaded: LoadedVersion<E>,
    ) -> Arc<LoadedVersion<E>> {
        let loaded = Arc::new(loaded);
        let shared: Arc<dyn Any + Send + Sync> = loaded.clone();
        self.entries.insert(version, Entry { graph_id, version: shared });
        loaded
    }

    /// The cached version ids, ascending.
    pub(crate) fn versions(&self) -> Vec<VersionId> {
        let mut versions: Vec<_> = self.entries.keys().copied().collect();
        versions.sort_unstable();
        versions
    }

    /// Drops every cached version not in `existing`.
    pub(crate) fn retain(&mut self, existing: &[VersionId]) {
        self.entries.retain(|version, _| existing.contains(version));
    }

    /// Drops every cached version of graph `graph_id`; returns how many.
    pub(crate) fn evict_graph(&mut self, graph_id: i32) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, entry| entry.graph_id != graph_id);
        before - self.entries.len()
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use super::*;
    use crate::cache::load;
    use crate::segment::{write_version, MemorySegmentStore};
    use warren_core::graph::{EdgeIntake, GraphCsr, IdMap, RelationDictionary};

    /// A store holding versions 1 and 2 (ids 1, 2, 3; edge 1 → 2) and 3
    /// (uuid ids).
    fn store() -> MemorySegmentStore {
        let mut store = MemorySegmentStore::default();
        let id_map = IdMap::<i64>::new().rebuild(vec![1, 2, 3]).unwrap();
        let mut relations = RelationDictionary::new();
        let relation = relations.intern("").unwrap();
        let mut intake = EdgeIntake::directed(&id_map, false);
        intake.push(relation, Some(&1), Some(&2), 1.0).unwrap();
        let graph = GraphCsr::build(intake.finish().0, 1, 3, true).unwrap();
        for version in [1, 2] {
            write_version(&mut store, version, &id_map, &relations, &graph, 64).unwrap();
        }
        let uuids = IdMap::<[u8; 16]>::new().rebuild(vec![[1; 16]]).unwrap();
        let empty =
            GraphCsr::build(EdgeIntake::directed(&uuids, false).finish().0, 1, 1, true).unwrap();
        write_version(&mut store, 3, &uuids, &relations, &empty, 64).unwrap();
        store
    }

    #[test]
    fn a_loaded_version_is_cached_and_shared() {
        let mut store = store();
        let mut cache = VersionCache::default();
        assert!(cache.get::<i64>(1).is_none());
        let loaded = cache.insert(10, 1, load::<i64>(&mut store, 1).unwrap());
        assert_eq!(loaded.id_map.dense_id(&2), Some(1));
        let cached = cache.get::<i64>(1).unwrap();
        assert!(Arc::ptr_eq(&loaded, &cached));
    }

    #[test]
    fn a_cached_version_is_found_only_with_its_id_type() {
        let mut cache = VersionCache::default();
        cache.insert(11, 3, load::<[u8; 16]>(&mut store(), 3).unwrap());
        assert!(cache.get::<[u8; 16]>(3).is_some());
        assert!(cache.get::<i64>(3).is_none());
    }

    #[test]
    fn versions_that_no_longer_exist_are_pruned() {
        let mut store = store();
        let mut cache = VersionCache::default();
        cache.insert(10, 1, load::<i64>(&mut store, 1).unwrap());
        cache.insert(10, 2, load::<i64>(&mut store, 2).unwrap());
        assert_eq!(cache.versions(), [1, 2]);
        cache.retain(&[2, 5]);
        assert_eq!(cache.versions(), [2]);
    }

    #[test]
    fn evicting_a_graph_drops_only_its_versions_and_spares_holders() {
        let mut store = store();
        let mut cache = VersionCache::default();
        let held = cache.insert(10, 1, load::<i64>(&mut store, 1).unwrap());
        cache.insert(10, 2, load::<i64>(&mut store, 2).unwrap());
        cache.insert(11, 3, load::<[u8; 16]>(&mut store, 3).unwrap());
        assert_eq!(cache.evict_graph(10), 2);
        assert_eq!(cache.versions(), [3]);
        assert_eq!(held.id_map.node_count(), 3, "a scan holding a version keeps it");
    }
}
