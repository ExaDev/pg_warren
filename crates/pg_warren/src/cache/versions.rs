//! The backend-local map of loaded versions, keyed by version id
//! (DESIGN.md §5.5). A version is immutable once built, so an entry never
//! goes stale; it only becomes unreachable when its version is dropped,
//! and is then pruned, or evicted to fit `warren.max_cache_memory`.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Instant;

use warren_core::graph::ExternalId;

use super::lru::Held;
use super::LoadedVersion;
use crate::catalog::VersionId;

thread_local! {
    /// This backend's loaded versions.
    pub(super) static CACHE: RefCell<VersionCache> = RefCell::new(VersionCache::default());
}

/// One cached version: an `Arc<LoadedVersion<E>>` for the graph's id type.
struct Entry {
    graph_id: i32,
    version: Arc<dyn Any + Send + Sync>,
    bytes: u64,
    last_used: Instant,
}

/// Loaded versions by version id. Callers hold `Arc` clones, so evicting a
/// version a scan is still reading is safe: the scan keeps its copy.
#[derive(Default)]
pub(crate) struct VersionCache {
    entries: HashMap<VersionId, Entry>,
    /// Versions evicted while a call still held them, by graph: a lookup
    /// in that call (a second vector of the same version) shares the copy
    /// it holds instead of loading another beside it. Not counted against
    /// the cap; gone once no call holds them, or once their versions are
    /// dropped or their graph evicted.
    held_by_calls: HashMap<VersionId, (i32, Weak<dyn Any + Send + Sync>)>,
}

impl VersionCache {
    /// Version `version`, if it is cached with external id type `E` (the
    /// lookup marks it used) or was evicted while a call still holds it.
    pub(crate) fn get<E: ExternalId + Send + Sync + 'static>(
        &mut self,
        version: VersionId,
    ) -> Option<Arc<LoadedVersion<E>>> {
        let Some(entry) = self.entries.get_mut(&version) else {
            let (_, held) = self.held_by_calls.get(&version)?;
            let held = held.upgrade();
            if held.is_none() {
                self.held_by_calls.remove(&version);
            }
            return held?.downcast().ok();
        };
        let loaded = Arc::clone(&entry.version).downcast().ok()?;
        entry.last_used = Instant::now();
        Some(loaded)
    }

    /// Caches `loaded` as `version` of graph `graph_id`, replacing any
    /// entry for it, and returns it shared.
    pub(crate) fn insert<E: ExternalId + Send + Sync + 'static>(
        &mut self,
        graph_id: i32,
        version: VersionId,
        loaded: LoadedVersion<E>,
    ) -> Arc<LoadedVersion<E>> {
        let bytes = loaded.bytes();
        let loaded = Arc::new(loaded);
        let shared: Arc<dyn Any + Send + Sync> = loaded.clone();
        let entry = Entry { graph_id, version: shared, bytes, last_used: Instant::now() };
        self.entries.insert(version, entry);
        self.held_by_calls.remove(&version);
        loaded
    }

    /// The heap bytes the cached versions hold.
    pub(crate) fn bytes(&self) -> u64 {
        self.entries.values().map(|entry| entry.bytes).sum()
    }

    /// Each cached version's bytes and last use, by version id.
    pub(crate) fn held(&self) -> Vec<Held<VersionId>> {
        let mut held: Vec<_> = self
            .entries
            .iter()
            .map(|(version, entry)| Held {
                key: *version,
                bytes: entry.bytes,
                last_used: entry.last_used,
            })
            .collect();
        held.sort_unstable_by_key(|entry| entry.key);
        held
    }

    /// Drops version `version`, if cached. A caller holding it keeps it,
    /// and a later lookup finds that copy while it is held.
    pub(crate) fn remove(&mut self, version: VersionId) {
        self.held_by_calls.retain(|_, (_, held)| held.strong_count() > 0);
        if let Some(entry) = self.entries.remove(&version) {
            self.held_by_calls.insert(version, (entry.graph_id, Arc::downgrade(&entry.version)));
        }
    }

    /// The version ids a prune must check, ascending: those cached, and
    /// those evicted that a call still holds.
    pub(crate) fn known_versions(&self) -> Vec<VersionId> {
        let held = self.held_by_calls.iter().filter(|(_, (_, held))| held.strong_count() > 0);
        let mut versions: Vec<_> =
            self.entries.keys().chain(held.map(|(version, _)| version)).copied().collect();
        versions.sort_unstable();
        versions
    }

    /// Drops every cached version not in `existing`.
    pub(crate) fn retain(&mut self, existing: &[VersionId]) {
        self.entries.retain(|version, _| existing.contains(version));
        self.held_by_calls.retain(|version, _| existing.contains(version));
    }

    /// Drops every cached version of graph `graph_id`; returns how many.
    pub(crate) fn evict_graph(&mut self, graph_id: i32) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, entry| entry.graph_id != graph_id);
        self.held_by_calls.retain(|_, (graph, _)| *graph != graph_id);
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

    /// The cached (not merely held) version ids, ascending.
    fn cached(cache: &VersionCache) -> Vec<VersionId> {
        cache.held().iter().map(|entry| entry.key).collect()
    }

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
        let loaded = cache.insert(10, 1, load::<i64>(&mut store, 1, true).unwrap());
        assert_eq!(loaded.id_map.dense_id(&2), Some(1));
        let cached = cache.get::<i64>(1).unwrap();
        assert!(Arc::ptr_eq(&loaded, &cached));
    }

    #[test]
    fn a_lookup_marks_a_version_used_and_held_reports_its_bytes() {
        let mut store = store();
        let mut cache = VersionCache::default();
        let first = cache.insert(10, 1, load::<i64>(&mut store, 1, true).unwrap());
        cache.insert(10, 2, load::<i64>(&mut store, 2, true).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(1));
        assert!(cache.get::<i64>(1).is_some());
        let held = cache.held();
        assert_eq!(held.iter().map(|entry| entry.key).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(held[0].bytes, first.bytes());
        assert!(held[0].bytes > 0);
        assert!(held[0].last_used > held[1].last_used, "the lookup marked version 1 used");
        cache.remove(1);
        assert_eq!(cached(&cache), [2]);
    }

    /// A call that reads a version twice (`vector_norm_diff` of two
    /// vectors of one version) must share the copy it holds even when the
    /// cap evicted it after the first load, not load a second copy.
    #[test]
    fn an_evicted_version_a_call_still_holds_is_found_again_without_a_reload() {
        let mut store = store();
        let mut cache = VersionCache::default();
        let held = cache.insert(10, 1, load::<i64>(&mut store, 1, true).unwrap());
        cache.remove(1);
        assert_eq!(cache.bytes(), 0, "the cache no longer counts it");
        let again = cache.get::<i64>(1).expect("the held copy");
        assert!(Arc::ptr_eq(&held, &again));
        assert!(cached(&cache).is_empty(), "found again, but not cached again");
        drop((held, again));
        assert!(cache.get::<i64>(1).is_none(), "gone once no call holds it");
    }

    #[test]
    fn evicting_a_graph_or_pruning_forgets_its_held_versions_too() {
        let mut store = store();
        let mut cache = VersionCache::default();
        let one = cache.insert(10, 1, load::<i64>(&mut store, 1, true).unwrap());
        let two = cache.insert(10, 2, load::<i64>(&mut store, 2, true).unwrap());
        cache.remove(1);
        cache.remove(2);
        assert!(cache.get::<i64>(1).is_some(), "held before the eviction");
        cache.evict_graph(10);
        assert!(cache.get::<i64>(1).is_none());
        assert!(cache.get::<i64>(2).is_none());
        let reloaded = cache.insert(10, 2, load::<i64>(&mut store, 2, true).unwrap());
        cache.remove(2);
        assert!(cache.get::<i64>(2).is_some(), "held before the prune");
        cache.retain(&[]);
        assert!(cache.get::<i64>(2).is_none());
        drop((one, two, reloaded));
    }

    /// A prune at another version's cache miss must not forget a version a
    /// call still holds while that version exists.
    #[test]
    fn a_prune_checks_held_versions_and_keeps_those_that_exist() {
        let mut store = store();
        let mut cache = VersionCache::default();
        let held = cache.insert(10, 1, load::<i64>(&mut store, 1, true).unwrap());
        cache.remove(1);
        cache.insert(10, 2, load::<i64>(&mut store, 2, true).unwrap());
        assert_eq!(cache.known_versions(), [1, 2]);
        cache.retain(&cache.known_versions());
        let again = cache.get::<i64>(1).expect("the held copy, after the prune");
        assert!(Arc::ptr_eq(&held, &again));
        drop((held, again));
        assert_eq!(cache.known_versions(), [2], "no longer held");
    }

    #[test]
    fn a_cached_version_is_found_only_with_its_id_type() {
        let mut cache = VersionCache::default();
        cache.insert(11, 3, load::<[u8; 16]>(&mut store(), 3, true).unwrap());
        assert!(cache.get::<[u8; 16]>(3).is_some());
        assert!(cache.get::<i64>(3).is_none());
    }

    #[test]
    fn versions_that_no_longer_exist_are_pruned() {
        let mut store = store();
        let mut cache = VersionCache::default();
        cache.insert(10, 1, load::<i64>(&mut store, 1, true).unwrap());
        cache.insert(10, 2, load::<i64>(&mut store, 2, true).unwrap());
        assert_eq!(cached(&cache), [1, 2]);
        cache.retain(&[2, 5]);
        assert_eq!(cached(&cache), [2]);
    }

    #[test]
    fn evicting_a_graph_drops_only_its_versions_and_spares_holders() {
        let mut store = store();
        let mut cache = VersionCache::default();
        let held = cache.insert(10, 1, load::<i64>(&mut store, 1, true).unwrap());
        cache.insert(10, 2, load::<i64>(&mut store, 2, true).unwrap());
        cache.insert(11, 3, load::<[u8; 16]>(&mut store, 3, true).unwrap());
        assert_eq!(cache.evict_graph(10), 2);
        assert_eq!(cached(&cache), [3]);
        assert_eq!(held.id_map.node_count(), 3, "a scan holding a version keeps it");
    }
}
