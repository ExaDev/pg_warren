//! Transient vectors (DESIGN.md §6.2): vectors written with
//! `persist := false`, kept only in this backend's memory, keyed by
//! (graph, version, role, name), never written to `warren.vectors`. The
//! role is the current user when written and read, so roles sharing a
//! backend (`SET ROLE`, a pooler, a `SECURITY DEFINER` function) can
//! neither see, shadow, replace nor drop each other's.
//!
//! They are not transactional: a ROLLBACK neither removes one written in
//! the transaction nor restores one it replaced. They do not pin their
//! version against retention. They count against `warren.max_cache_memory`
//! but, since they cannot be reloaded, are never evicted to fit it. They are freed at session end, by
//! `warren.evict`, by `drop_vector`, by a persisted write of the same name,
//! and when a later cache load finds their version gone.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use pgrx::pg_sys;
use warren_core::compute::AnyVector;

use crate::catalog::VersionId;

/// A transient vector's key: graph, version, role and name.
type Key = (i32, VersionId, u32, String);

/// The current role: whose transient vectors a call sees.
fn role() -> u32 {
    // SAFETY: reads the backend's current user id.
    u32::from(unsafe { pg_sys::GetUserId() })
}

thread_local! {
    /// This backend's transient vectors.
    static TRANSIENT: RefCell<BTreeMap<Key, Arc<AnyVector>>> =
        const { RefCell::new(BTreeMap::new()) };
}

/// The current role's transient vector `name` of version `version` of
/// graph `graph_id`.
pub(crate) fn transient(graph_id: i32, version: VersionId, name: &str) -> Option<Arc<AnyVector>> {
    TRANSIENT.with_borrow(|map| map.get(&(graph_id, version, role(), name.to_owned())).cloned())
}

/// The current role's transient vectors of version `version` of graph
/// `graph_id`, by name.
pub(crate) fn transients(graph_id: i32, version: VersionId) -> Vec<(String, Arc<AnyVector>)> {
    let role = role();
    TRANSIENT.with_borrow(|map| {
        map.range((graph_id, version, role, String::new())..)
            .take_while(|((graph, at, owner, _), _)| {
                (*graph, *at, *owner) == (graph_id, version, role)
            })
            .map(|((_, _, _, name), vector)| (name.clone(), Arc::clone(vector)))
            .collect()
    })
}

/// Keeps `vector` as the current role's transient vector `name`, replacing
/// any of that name.
pub(crate) fn keep_transient(
    graph_id: i32,
    version: VersionId,
    name: &str,
    vector: Arc<AnyVector>,
) {
    let key = (graph_id, version, role(), name.to_owned());
    TRANSIENT.with_borrow_mut(|map| map.insert(key, vector));
}

/// Drops the current role's transient vector `name`; returns whether there
/// was one.
pub(crate) fn drop_transient(graph_id: i32, version: VersionId, name: &str) -> bool {
    let key = (graph_id, version, role(), name.to_owned());
    TRANSIENT.with_borrow_mut(|map| map.remove(&key).is_some())
}

/// The heap bytes this backend's transient vectors (of every role) hold.
pub(crate) fn transient_bytes() -> u64 {
    TRANSIENT.with_borrow(|map| map.values().map(|vector| vector.bytes()).sum())
}

/// The current role's transient vectors: each one's graph, version, name,
/// bytes and whether a call holds it.
pub(crate) fn role_transients() -> Vec<(i32, VersionId, String, u64, bool)> {
    let role = role();
    TRANSIENT.with_borrow(|map| {
        map.iter()
            .filter(|((_, _, owner, _), _)| *owner == role)
            .map(|((graph, version, _, name), vector)| {
                let in_use = Arc::strong_count(vector) > 1;
                (*graph, *version, name.clone(), vector.bytes(), in_use)
            })
            .collect()
    })
}

/// How many transient vectors (of every role) this backend holds.
pub(crate) fn transient_count() -> usize {
    TRANSIENT.with_borrow(BTreeMap::len)
}

/// The heap bytes the current role's transient vectors hold.
pub(crate) fn role_transient_bytes() -> u64 {
    let role = role();
    TRANSIENT.with_borrow(|map| {
        map.iter()
            .filter(|((_, _, owner, _), _)| *owner == role)
            .map(|(_, vector)| vector.bytes())
            .sum()
    })
}

/// The versions that have transient vectors (of any role), ascending.
pub(crate) fn transient_versions() -> Vec<VersionId> {
    let mut versions: Vec<VersionId> =
        TRANSIENT.with_borrow(|map| map.keys().map(|(_, version, _, _)| *version).collect());
    versions.dedup();
    versions.sort_unstable();
    versions.dedup();
    versions
}

/// Drops the transient vectors (of every role) of every version not in
/// `existing`: those versions were dropped.
pub(crate) fn retain_transients(existing: &[VersionId]) {
    TRANSIENT.with_borrow_mut(|map| map.retain(|(_, version, _, _), _| existing.contains(version)));
}

/// Drops the current role's transient vectors of graph `graph_id`; returns
/// how many.
pub(crate) fn evict_transients(graph_id: i32) -> usize {
    let role = role();
    TRANSIENT.with_borrow_mut(|map| {
        let before = map.len();
        map.retain(|(graph, _, owner, _), _| (*graph, *owner) != (graph_id, role));
        before - map.len()
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use super::*;
    use pgrx::prelude::*;
    use pgrx::spi::Error;
    use warren_core::compute::{ComputeMemory, Vector};

    fn vector(value: f64) -> Result<Arc<AnyVector>, Error> {
        let memory = ComputeMemory::new(1 << 20);
        let values = Vector::filled(&memory, 2, 1, value).map_err(|_| Error::InvalidPosition)?;
        Ok(Arc::new(values.into()))
    }

    #[pg_test]
    fn transients_are_kept_per_graph_version_role_and_name() -> Result<(), Error> {
        keep_transient(1, 10, "b", vector(1.0)?);
        keep_transient(1, 10, "a", vector(2.0)?);
        keep_transient(1, 11, "a", vector(3.0)?);
        keep_transient(2, 10, "a", vector(4.0)?);
        Spi::run("CREATE ROLE other; SET ROLE other")?;
        keep_transient(1, 10, "a", vector(5.0)?);
        let all_roles = transient_bytes();
        let own = role_transient_bytes();
        let other: Vec<String> = transients(1, 10).into_iter().map(|(name, _)| name).collect();
        assert_eq!(evict_transients(1), 1, "another role evicts only its own");
        Spi::run("RESET ROLE")?;
        assert_eq!(other, ["a"]);
        assert_eq!(all_roles, 5 * 16, "every role's transient vectors count");
        assert_eq!(own, 16);
        let names: Vec<String> = transients(1, 10).into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(transient(1, 10, "a"), Some(vector(2.0)?));
        assert_eq!(transient(1, 11, "a"), Some(vector(3.0)?));
        assert_eq!(transient_versions(), [10, 11]);
        retain_transients(&[10]);
        assert_eq!(transient(1, 11, "a"), None);
        assert!(drop_transient(1, 10, "b"));
        assert!(!drop_transient(1, 10, "b"));
        assert_eq!(evict_transients(1), 1);
        assert_eq!(evict_transients(2), 1);
        assert!(transient_versions().is_empty());
        Ok(())
    }
}
