//! The backend-local map of loaded versions, keyed by version id
//! (DESIGN.md §5.5). A version is immutable once built, so an entry never
//! goes stale; it only becomes unreachable when its version is dropped,
//! and is then pruned, or evicted to fit `warren.max_cache_memory`.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Instant;

use warren_core::graph::IdKeys;

use super::lru::Held;
use super::LoadedVersion;
use crate::activity::count_private_evictions;
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

/// A version evicted while a call still holds it.
struct HeldByCall {
    graph_id: i32,
    version: Weak<dyn Any + Send + Sync>,
    bytes: u64,
}

impl HeldByCall {
    /// Whether a call still holds it.
    fn held(&self) -> bool {
        self.version.strong_count() > 0
    }
}

/// A version the cache lists: cached, or evicted while a call holds it
/// (no last use).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    pub(crate) version: VersionId,
    pub(crate) graph_id: i32,
    pub(crate) bytes: u64,
    pub(crate) last_used: Option<Instant>,
    /// Whether a call holds it.
    pub(crate) in_use: bool,
}

/// Loaded versions by version id. Callers hold `Arc` clones, so evicting a
/// version a scan is still reading is safe: the scan keeps its copy.
#[derive(Default)]
pub(crate) struct VersionCache {
    entries: HashMap<VersionId, Entry>,
    /// Versions evicted while a call still held them, by graph: a lookup
    /// while one is held (after `warren.evict`, or in a later statement
    /// than the scan holding it) caches that copy again instead of loading
    /// another beside it. They count against the limits while held (DESIGN
    /// §5.5); gone once no call holds them, or once their versions are
    /// dropped or their graph evicted.
    held_by_calls: HashMap<VersionId, HeldByCall>,
}

impl VersionCache {
    /// Version `version`, if it is cached with key storage `E` (the
    /// lookup marks it used) or was evicted while a call still holds it
    /// (the lookup caches it again).
    pub(crate) fn get<E: IdKeys + Send + Sync + 'static>(
        &mut self,
        version: VersionId,
    ) -> Option<Arc<LoadedVersion<E>>> {
        if let Some(entry) = self.entries.get_mut(&version) {
            entry.last_used = Instant::now();
            return Arc::clone(&entry.version).downcast().ok();
        }
        let held_by_call = self.held_by_calls.remove(&version)?;
        let held: Arc<LoadedVersion<E>> = held_by_call.version.upgrade()?.downcast().ok()?;
        self.cache(held_by_call.graph_id, version, Arc::clone(&held));
        Some(held)
    }

    /// Whether a call holds cached version `version`: it is shared beyond
    /// the cache's own reference.
    pub(crate) fn in_use(&self, version: VersionId) -> bool {
        self.entries.get(&version).is_some_and(|entry| Arc::strong_count(&entry.version) > 1)
    }

    /// The bytes of the versions evicted that calls still hold.
    pub(crate) fn held_by_calls_bytes(&self) -> u64 {
        self.held_by_calls.values().filter(|held| held.held()).map(|held| held.bytes).sum()
    }

    /// Caches `loaded` as `version` of graph `graph_id`, replacing any
    /// entry for it, and returns it shared.
    pub(crate) fn insert<E: IdKeys + Send + Sync + 'static>(
        &mut self,
        graph_id: i32,
        version: VersionId,
        loaded: LoadedVersion<E>,
    ) -> Arc<LoadedVersion<E>> {
        let loaded = Arc::new(loaded);
        self.cache(graph_id, version, Arc::clone(&loaded));
        loaded
    }

    /// Caches `loaded` as `version` of graph `graph_id`.
    fn cache<E: IdKeys + Send + Sync + 'static>(
        &mut self,
        graph_id: i32,
        version: VersionId,
        loaded: Arc<LoadedVersion<E>>,
    ) {
        let bytes = loaded.bytes();
        let entry = Entry { graph_id, version: loaded, bytes, last_used: Instant::now() };
        self.entries.insert(version, entry);
        self.held_by_calls.remove(&version);
    }

    /// The heap bytes the cached versions hold.
    pub(crate) fn bytes(&self) -> u64 {
        self.entries.values().map(|entry| entry.bytes).sum()
    }

    /// How many versions are cached.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
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

    /// Every version cached, and every version evicted that a call still
    /// holds, for `warren.cache()`.
    pub(crate) fn listing(&self) -> Vec<Listed> {
        let cached = self.entries.iter().map(|(version, entry)| Listed {
            version: *version,
            graph_id: entry.graph_id,
            bytes: entry.bytes,
            last_used: Some(entry.last_used),
            in_use: Arc::strong_count(&entry.version) > 1,
        });
        let held =
            self.held_by_calls.iter().filter(|(_, held)| held.held()).map(|(version, held)| {
                Listed {
                    version: *version,
                    graph_id: held.graph_id,
                    bytes: held.bytes,
                    last_used: None,
                    in_use: true,
                }
            });
        cached.chain(held).collect()
    }

    /// Drops version `version`, if cached, counting the eviction (DESIGN.md
    /// §5.7). A caller holding it keeps it, and a later lookup finds that
    /// copy while it is held.
    pub(crate) fn remove(&mut self, version: VersionId) {
        self.held_by_calls.retain(|_, held| held.held());
        if let Some(entry) = self.entries.remove(&version) {
            count_private_evictions(1);
            let held = HeldByCall {
                graph_id: entry.graph_id,
                version: Arc::downgrade(&entry.version),
                bytes: entry.bytes,
            };
            self.held_by_calls.insert(version, held);
        }
    }

    /// The version ids a prune must check, ascending: those cached, and
    /// those evicted that a call still holds.
    pub(crate) fn known_versions(&self) -> Vec<VersionId> {
        let held = self.held_by_calls.iter().filter(|(_, held)| held.held());
        let mut versions: Vec<_> =
            self.entries.keys().chain(held.map(|(version, _)| version)).copied().collect();
        versions.sort_unstable();
        versions
    }

    /// Drops every cached version not in `existing`. Versions calls still
    /// hold stay counted until they let go, whether or not they exist (one
    /// that does not can never be looked up again).
    pub(crate) fn retain(&mut self, existing: &[VersionId]) {
        let gone: Vec<VersionId> =
            self.entries.keys().filter(|version| !existing.contains(version)).copied().collect();
        for version in gone {
            self.remove(version);
        }
        self.held_by_calls.retain(|_, held| held.held());
    }

    /// Drops every cached version of graph `graph_id` except those `kept`.
    pub(crate) fn evict_graph_except(&mut self, graph_id: i32, kept: impl Fn(VersionId) -> bool) {
        let evicted: Vec<VersionId> = self
            .entries
            .iter()
            .filter(|(version, entry)| entry.graph_id == graph_id && !kept(**version))
            .map(|(version, _)| *version)
            .collect();
        for version in evicted {
            self.remove(version);
        }
    }

    /// Drops every cached version of graph `graph_id`; returns how many.
    /// Those calls still hold stay counted, and shared, until they let go.
    pub(crate) fn evict_graph(&mut self, graph_id: i32) -> usize {
        let evicted: Vec<VersionId> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.graph_id == graph_id)
            .map(|(version, _)| *version)
            .collect();
        for &version in &evicted {
            self.remove(version);
        }
        evicted.len()
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use super::*;
    use crate::cache::load;
    use crate::segment::{write_version, MemorySegmentStore};
    use warren_core::graph::{EdgeIntake, FixedKeys, GraphCsr, IdMap, RelationDictionary};

    /// The cached (not merely held) version ids, ascending.
    fn cached(cache: &VersionCache) -> Vec<VersionId> {
        cache.held().iter().map(|entry| entry.key).collect()
    }

    /// A store holding versions 1 and 2 (ids 1, 2, 3; edge 1 → 2) and 3
    /// (uuid ids).
    fn store() -> MemorySegmentStore {
        let mut store = MemorySegmentStore::default();
        let id_map = IdMap::<FixedKeys<i64>>::new().rebuild(vec![1, 2, 3]).unwrap();
        let mut relations = RelationDictionary::new();
        let relation = relations.intern("").unwrap();
        let mut intake = EdgeIntake::directed(&id_map, false);
        intake.push(relation, Some(&1), Some(&2), 1.0).unwrap();
        let graph = GraphCsr::build(intake.finish().0, 1, 3, true).unwrap();
        for version in [1, 2] {
            write_version(&mut store, version, &id_map, &relations, &graph, 64).unwrap();
        }
        let uuids = IdMap::<FixedKeys<[u8; 16]>>::new().rebuild(vec![[1; 16]]).unwrap();
        let empty =
            GraphCsr::build(EdgeIntake::directed(&uuids, false).finish().0, 1, 1, true).unwrap();
        write_version(&mut store, 3, &uuids, &relations, &empty, 64).unwrap();
        store
    }

    #[test]
    fn a_loaded_version_is_cached_and_shared() {
        let mut store = store();
        let mut cache = VersionCache::default();
        assert!(cache.get::<FixedKeys<i64>>(1).is_none());
        let loaded = cache.insert(10, 1, load::<FixedKeys<i64>>(&mut store, 1, true).unwrap());
        assert_eq!(loaded.id_map.dense_id(&2), Some(1));
        let cached = cache.get::<FixedKeys<i64>>(1).unwrap();
        assert!(Arc::ptr_eq(&loaded, &cached));
    }

    #[test]
    fn a_lookup_marks_a_version_used_and_held_reports_its_bytes() {
        let mut store = store();
        let mut cache = VersionCache::default();
        let first = cache.insert(10, 1, load::<FixedKeys<i64>>(&mut store, 1, true).unwrap());
        cache.insert(10, 2, load::<FixedKeys<i64>>(&mut store, 2, true).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(1));
        assert!(cache.get::<FixedKeys<i64>>(1).is_some());
        let held = cache.held();
        assert_eq!(held.iter().map(|entry| entry.key).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(held[0].bytes, first.bytes());
        assert!(held[0].bytes > 0);
        assert!(held[0].last_used > held[1].last_used, "the lookup marked version 1 used");
        cache.remove(1);
        assert_eq!(cached(&cache), [2]);
    }

    /// A lookup of a version evicted while a call still holds it (after
    /// `warren.evict`, or from a scan of an earlier statement) must cache
    /// the copy held, not load a second copy beside it.
    #[test]
    fn an_evicted_version_a_call_still_holds_is_cached_again_without_a_reload() {
        let mut store = store();
        let mut cache = VersionCache::default();
        let held = cache.insert(10, 1, load::<FixedKeys<i64>>(&mut store, 1, true).unwrap());
        cache.remove(1);
        assert_eq!(cache.bytes(), 0, "the cache no longer counts it");
        assert_eq!(cache.held_by_calls_bytes(), held.bytes(), "counted while held");
        let again = cache.get::<FixedKeys<i64>>(1).expect("the held copy");
        assert!(Arc::ptr_eq(&held, &again));
        assert_eq!(cached(&cache), [1], "cached again");
        assert_eq!(cache.bytes(), held.bytes(), "and counted again");
        cache.remove(1);
        drop((held, again));
        assert_eq!(cache.held_by_calls_bytes(), 0);
        assert!(cache.get::<FixedKeys<i64>>(1).is_none(), "gone once no call holds it");
    }

    #[test]
    fn evicting_a_graph_except_some_versions_keeps_those() {
        let mut store = store();
        let mut cache = VersionCache::default();
        cache.insert(10, 1, load::<FixedKeys<i64>>(&mut store, 1, true).unwrap());
        cache.insert(10, 2, load::<FixedKeys<i64>>(&mut store, 2, true).unwrap());
        cache.insert(11, 3, load::<FixedKeys<[u8; 16]>>(&mut store, 3, true).unwrap());
        cache.evict_graph_except(10, |version| version == 2);
        assert_eq!(cached(&cache), [2, 3]);
    }

    /// What `warren.evict` or a prune drops, a call may still hold: it
    /// stays counted against the limits (and an evicted one shared) until
    /// the call lets it go.
    #[test]
    fn evicting_a_graph_or_pruning_keeps_what_calls_hold_counted() {
        let mut store = store();
        let mut cache = VersionCache::default();
        let one = cache.insert(10, 1, load::<FixedKeys<i64>>(&mut store, 1, true).unwrap());
        cache.insert(10, 2, load::<FixedKeys<i64>>(&mut store, 2, true).unwrap());
        assert_eq!(cache.evict_graph(10), 2);
        assert_eq!((cache.bytes(), cache.held_by_calls_bytes()), (0, one.bytes()));
        assert!(
            Arc::ptr_eq(&one, &cache.get::<FixedKeys<i64>>(1).unwrap()),
            "shared, not reloaded"
        );
        assert!(cache.get::<FixedKeys<i64>>(2).is_none(), "no call held version 2");
        cache.retain(&[]);
        assert_eq!((cache.bytes(), cache.held_by_calls_bytes()), (0, one.bytes()), "pruned, held");
        drop(one);
        assert_eq!(cache.held_by_calls_bytes(), 0, "let go");
    }

    /// A prune at another version's cache miss must not forget a version a
    /// call still holds while that version exists.
    #[test]
    fn a_prune_checks_held_versions_and_keeps_those_that_exist() {
        let mut store = store();
        let mut cache = VersionCache::default();
        let held = cache.insert(10, 1, load::<FixedKeys<i64>>(&mut store, 1, true).unwrap());
        cache.remove(1);
        cache.insert(10, 2, load::<FixedKeys<i64>>(&mut store, 2, true).unwrap());
        assert_eq!(cache.known_versions(), [1, 2]);
        cache.retain(&cache.known_versions());
        let again = cache.get::<FixedKeys<i64>>(1).expect("the held copy, after the prune");
        assert!(Arc::ptr_eq(&held, &again));
        assert_eq!(cached(&cache), [1, 2], "found again, so cached again");
        cache.remove(1);
        drop((held, again));
        assert_eq!(cache.known_versions(), [2], "no longer held");
    }

    #[test]
    fn the_listing_shows_cached_versions_and_those_calls_hold() {
        let mut store = store();
        let mut cache = VersionCache::default();
        let held = cache.insert(10, 1, load::<FixedKeys<i64>>(&mut store, 1, true).unwrap());
        cache.insert(10, 2, load::<FixedKeys<i64>>(&mut store, 2, true).unwrap());
        let mut listing = cache.listing();
        listing.sort_by_key(|listed| listed.version);
        let shown: Vec<_> = listing.iter().map(|l| (l.version, l.in_use)).collect();
        assert_eq!(shown, [(1, true), (2, false)], "a call holds version 1");
        cache.remove(1);
        let listing = cache.listing();
        let evicted = listing.iter().find(|listed| listed.version == 1).unwrap();
        assert_eq!((evicted.last_used, evicted.in_use), (None, true), "held, no longer cached");
        assert_eq!(evicted.bytes, held.bytes());
        drop(held);
        assert_eq!(cache.listing().len(), 1, "gone once no call holds it");
    }

    #[test]
    fn a_cached_version_is_found_only_with_its_id_type() {
        let mut cache = VersionCache::default();
        cache.insert(11, 3, load::<FixedKeys<[u8; 16]>>(&mut store(), 3, true).unwrap());
        assert!(cache.get::<FixedKeys<[u8; 16]>>(3).is_some());
        assert!(cache.get::<FixedKeys<i64>>(3).is_none());
    }

    #[test]
    fn versions_that_no_longer_exist_are_pruned() {
        let mut store = store();
        let mut cache = VersionCache::default();
        cache.insert(10, 1, load::<FixedKeys<i64>>(&mut store, 1, true).unwrap());
        cache.insert(10, 2, load::<FixedKeys<i64>>(&mut store, 2, true).unwrap());
        assert_eq!(cached(&cache), [1, 2]);
        cache.retain(&[2, 5]);
        assert_eq!(cached(&cache), [2]);
    }

    #[test]
    fn evicting_a_graph_drops_only_its_versions_and_spares_holders() {
        let mut store = store();
        let mut cache = VersionCache::default();
        let held = cache.insert(10, 1, load::<FixedKeys<i64>>(&mut store, 1, true).unwrap());
        cache.insert(10, 2, load::<FixedKeys<i64>>(&mut store, 2, true).unwrap());
        cache.insert(11, 3, load::<FixedKeys<[u8; 16]>>(&mut store, 3, true).unwrap());
        assert_eq!(cache.evict_graph(10), 2);
        assert_eq!(cached(&cache), [3]);
        assert_eq!(held.id_map.node_count(), 3, "a scan holding a version keeps it");
    }
}
