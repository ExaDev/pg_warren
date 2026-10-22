//! Evicting shared versions besides admission (DESIGN.md §5.6, §14 item
//! 34): `warren.evict` (the graph's versions, by a member of its owner
//! role), the load of a graph's current version (the graph's other
//! versions), and the prune of versions dropped since they were loaded.
//! Each evicts only unpinned versions (a mapped one drains until its
//! mappers release it), and releases this backend's own mappings of them
//! that no call holds.

use pgrx::pg_sys::Oid;

use super::slot::Key;
use super::SharedVersions;
use crate::catalog::VersionId;

/// Evicts `versions` if `owner` (the caller is a member of their graph's
/// owner role; a reader cannot evict what other sessions use), and
/// releases this backend's mappings of them either way. How many it
/// evicted.
pub(crate) fn evict_versions<P: SharedVersions>(
    port: &mut P,
    versions: &[Key],
    owner: bool,
) -> usize {
    let selected = |key: &Key| versions.contains(key);
    let evicted = if owner { port.evict(&selected) } else { 0 };
    port.forget(&selected);
    evicted
}

/// Evicts the shared versions of database `database` that no longer
/// exist: those of pg_warren's installation `extension` that `existing`
/// (given their ids) does not return, and every one of an earlier
/// installation, dropped with its extension. How many it evicted.
pub(crate) fn prune<P: SharedVersions, E>(
    port: &mut P,
    database: Oid,
    extension: Oid,
    existing: impl FnOnce(&[VersionId]) -> Result<Vec<VersionId>, E>,
) -> Result<usize, E> {
    let held: Vec<Key> = port.keys().into_iter().filter(|key| key.database == database).collect();
    if held.is_empty() {
        return Ok(0);
    }
    let ours: Vec<VersionId> =
        held.iter().filter(|key| key.extension == extension).map(|key| key.version).collect();
    let existing = if ours.is_empty() { Vec::new() } else { existing(&ours)? };
    let gone: Vec<Key> = held
        .into_iter()
        .filter(|key| key.extension != extension || !existing.contains(&key.version))
        .collect();
    Ok(if gone.is_empty() { 0 } else { evict_versions(port, &gone, true) })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::cache::shared::memory::{Backend, Mapping, Memory};
    use crate::cache::shared::slot::State;
    use crate::cache::shared::{Claimed, Found};

    fn key(extension: u32, version: i64) -> Key {
        Key { database: Oid::from(1), extension: Oid::from(extension), version }
    }

    /// Claims and publishes `key`'s version in `backend`; the view its
    /// call holds.
    fn published(backend: &mut Backend, key: Key) -> Arc<Mapping> {
        let Claimed::Load(region) = backend.claim(key, 10) else {
            panic!("not claimed");
        };
        backend.publish(region).unwrap()
    }

    fn states(memory: &Memory) -> Vec<(i64, State, u32)> {
        let mut states: Vec<_> = memory
            .slots()
            .iter()
            .filter(|slot| slot.state != State::Empty)
            .map(|slot| (slot.key.version, slot.state, slot.mappers))
            .collect();
        states.sort_unstable_by_key(|(version, _, _)| *version);
        states
    }

    #[test]
    fn an_owner_evicts_the_unpinned_versions_and_a_reader_only_forgets_its_mappings() {
        let memory = Memory::new(4, 100, u64::MAX, Duration::ZERO);
        let (mut loader, mut reader) = (memory.backend(), memory.backend());
        for version in [1, 2, 3] {
            drop(published(&mut loader, key(2, version)));
        }
        loader.unpin();
        let Found::Ready(view) = reader.lookup(key(2, 1)) else { panic!("not found") };
        drop(view);
        reader.unpin();
        assert_eq!(evict_versions(&mut reader, &[key(2, 1), key(2, 2)], false), 0);
        assert_eq!(
            states(&memory),
            [(1, State::Ready, 1), (2, State::Ready, 1), (3, State::Ready, 1)]
        );
        let Found::Ready(pinned) = reader.lookup(key(2, 2)) else { panic!("not found") };
        assert_eq!(evict_versions(&mut loader, &[key(2, 1), key(2, 2)], true), 1);
        assert_eq!(
            states(&memory),
            [(2, State::Ready, 1), (3, State::Ready, 1)],
            "the one a statement pins stays, mapped by its reader alone"
        );
        drop(pinned);
    }

    #[test]
    fn a_prune_evicts_what_no_longer_exists_and_earlier_installations() {
        let memory = Memory::new(6, 100, u64::MAX, Duration::ZERO);
        let mut backend = memory.backend();
        let other_database = Key { database: Oid::from(9), ..key(2, 1) };
        for key in [key(1, 1), key(2, 1), key(2, 2), key(2, 3), other_database] {
            drop(published(&mut backend, key));
        }
        backend.unpin();
        let mut asked = Vec::new();
        let pruned = prune(&mut backend, Oid::from(1), Oid::from(2), |versions| {
            asked = versions.to_vec();
            Ok::<_, ()>(vec![2])
        });
        asked.sort_unstable();
        assert_eq!(asked, [1, 2, 3], "only this installation's versions are looked up");
        assert_eq!(pruned, Ok(3));
        let left: Vec<Key> = memory
            .slots()
            .iter()
            .filter(|slot| slot.state != State::Empty)
            .map(|slot| slot.key)
            .collect();
        assert_eq!(left.len(), 2);
        assert!(left.contains(&key(2, 2)) && left.contains(&other_database));
    }

    #[test]
    fn a_prune_with_nothing_shared_reads_nothing() {
        let memory = Memory::new(2, 100, u64::MAX, Duration::ZERO);
        let pruned =
            prune(&mut memory.backend(), Oid::from(1), Oid::from(2), |_| -> Result<_, ()> {
                panic!("looked up")
            });
        assert_eq!(pruned, Ok(0));
    }
}
