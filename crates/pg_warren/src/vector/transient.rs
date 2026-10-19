//! Transient vectors (DESIGN.md §6.2): vectors written with
//! `persist := false`, kept only in this backend's memory, keyed by
//! (graph, version, name), never written to `warren.vectors`.
//!
//! They are not transactional: a ROLLBACK neither removes one written in
//! the transaction nor restores one it replaced. They do not pin their
//! version against retention. They are freed at session end, by
//! `warren.evict`, by `drop_vector`, by a persisted write of the same name,
//! and when a later cache load finds their version gone.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use warren_core::compute::AnyVector;

use crate::catalog::VersionId;

/// A transient vector's key: graph, version and name.
type Key = (i32, VersionId, String);

thread_local! {
    /// This backend's transient vectors.
    static TRANSIENT: RefCell<BTreeMap<Key, Arc<AnyVector>>> =
        const { RefCell::new(BTreeMap::new()) };
}

/// Transient vector `name` of version `version` of graph `graph_id`.
pub(crate) fn transient(graph_id: i32, version: VersionId, name: &str) -> Option<Arc<AnyVector>> {
    TRANSIENT.with_borrow(|map| map.get(&(graph_id, version, name.to_owned())).cloned())
}

/// The transient vectors of version `version` of graph `graph_id`, by name.
pub(crate) fn transients(graph_id: i32, version: VersionId) -> Vec<(String, Arc<AnyVector>)> {
    TRANSIENT.with_borrow(|map| {
        map.range((graph_id, version, String::new())..)
            .take_while(|((graph, at, _), _)| (*graph, *at) == (graph_id, version))
            .map(|((_, _, name), vector)| (name.clone(), Arc::clone(vector)))
            .collect()
    })
}

/// Keeps `vector` as transient vector `name`, replacing any of that name.
pub(crate) fn keep_transient(
    graph_id: i32,
    version: VersionId,
    name: &str,
    vector: Arc<AnyVector>,
) {
    TRANSIENT.with_borrow_mut(|map| map.insert((graph_id, version, name.to_owned()), vector));
}

/// Drops transient vector `name`; returns whether there was one.
pub(crate) fn drop_transient(graph_id: i32, version: VersionId, name: &str) -> bool {
    TRANSIENT.with_borrow_mut(|map| map.remove(&(graph_id, version, name.to_owned())).is_some())
}

/// The versions that have transient vectors, ascending.
pub(crate) fn transient_versions() -> Vec<VersionId> {
    let mut versions: Vec<VersionId> =
        TRANSIENT.with_borrow(|map| map.keys().map(|(_, version, _)| *version).collect());
    versions.dedup();
    versions.sort_unstable();
    versions.dedup();
    versions
}

/// Drops the transient vectors of every version not in `existing`.
pub(crate) fn retain_transients(existing: &[VersionId]) {
    TRANSIENT.with_borrow_mut(|map| map.retain(|(_, version, _), _| existing.contains(version)));
}

/// Drops the transient vectors of graph `graph_id`; returns how many.
pub(crate) fn evict_transients(graph_id: i32) -> usize {
    TRANSIENT.with_borrow_mut(|map| {
        let before = map.len();
        map.retain(|(graph, _, _), _| *graph != graph_id);
        before - map.len()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use warren_core::compute::{ComputeMemory, Vector};

    fn vector(value: f64) -> Arc<AnyVector> {
        let memory = ComputeMemory::new(1 << 20);
        Arc::new(Vector::filled(&memory, 2, 1, value).unwrap().into())
    }

    #[test]
    fn transients_are_kept_per_graph_version_and_name() {
        keep_transient(1, 10, "b", vector(1.0));
        keep_transient(1, 10, "a", vector(2.0));
        keep_transient(1, 11, "a", vector(3.0));
        keep_transient(2, 10, "a", vector(4.0));
        let names: Vec<String> = transients(1, 10).into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(transient(1, 11, "a"), Some(vector(3.0)));
        assert_eq!(transient_versions(), [10, 11]);
        retain_transients(&[10]);
        assert_eq!(transient(1, 11, "a"), None);
        assert!(drop_transient(1, 10, "b"));
        assert!(!drop_transient(1, 10, "b"));
        assert_eq!(evict_transients(1), 1);
        assert_eq!(evict_transients(2), 1);
        assert!(transient_versions().is_empty());
    }
}
