//! Evicting shared versions and persisted vectors besides admission
//! (DESIGN.md §5.6, §14 items 34 and 38): `warren.evict` (the graph's
//! versions and vectors, by a member of its owner role), the load of a
//! graph's current version (the graph's other versions), dropping or
//! replacing a vector, and the prune of versions and vectors deleted since
//! they were loaded.
//! Each evicts only unpinned versions (a mapped one drains until its
//! mappers release it), and releases this backend's own mappings of them
//! that no call holds.

use pgrx::pg_sys::Oid;

use super::slot::{Key, Kind};
use super::SharedVersions;

/// Evicts the shared versions and vectors `keys` names if `owner` (the
/// caller is a member of their graph's owner role, or a vector's creator;
/// a reader cannot evict what other sessions use), and releases this
/// backend's mappings of them either way. How many it evicted.
pub(crate) fn evict_shared<P: SharedVersions>(port: &mut P, keys: &[Key], owner: bool) -> usize {
    let selected = |key: &Key| keys.contains(key);
    let evicted = if owner { port.evict(&selected) } else { 0 };
    port.forget(&selected);
    evicted
}

/// Evicts the shared versions and persisted vectors of database
/// `database` that no longer exist: those of pg_warren's installation
/// `extension` that `existing` (given their kind and ids, one call per
/// kind held) does not return, and every one of an earlier installation,
/// dropped with its extension. How many it evicted.
pub(crate) fn prune<P: SharedVersions, E>(
    port: &mut P,
    database: Oid,
    extension: Oid,
    mut existing: impl FnMut(Kind, &[i64]) -> Result<Vec<i64>, E>,
) -> Result<usize, E> {
    let held: Vec<Key> = port.keys().into_iter().filter(|key| key.database == database).collect();
    let mut gone: Vec<Key> =
        held.iter().filter(|key| key.extension != extension).copied().collect();
    for kind in [Kind::Version, Kind::Vector] {
        let ours = |key: &&Key| key.extension == extension && key.kind == kind;
        let ids: Vec<i64> = held.iter().filter(ours).map(|key| key.id).collect();
        if ids.is_empty() {
            continue;
        }
        let existing = existing(kind, &ids)?;
        gone.extend(held.iter().filter(ours).filter(|key| !existing.contains(&key.id)));
    }
    Ok(if gone.is_empty() { 0 } else { evict_shared(port, &gone, true) })
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
        Key::version(Oid::from(1), Oid::from(extension), version)
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
            .map(|slot| (slot.key.id, slot.state, slot.mappers))
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
        assert_eq!(evict_shared(&mut reader, &[key(2, 1), key(2, 2)], false), 0);
        assert_eq!(
            states(&memory),
            [(1, State::Ready, 1), (2, State::Ready, 1), (3, State::Ready, 1)]
        );
        let Found::Ready(pinned) = reader.lookup(key(2, 2)) else { panic!("not found") };
        assert_eq!(evict_shared(&mut loader, &[key(2, 1), key(2, 2)], true), 1);
        assert_eq!(
            states(&memory),
            [(2, State::Ready, 1), (3, State::Ready, 1)],
            "the one a statement pins stays, mapped by its reader alone"
        );
        drop(pinned);
    }

    #[test]
    fn a_prune_evicts_what_no_longer_exists_and_earlier_installations() {
        let memory = Memory::new(8, 100, u64::MAX, Duration::ZERO);
        let mut backend = memory.backend();
        let other_database = Key { database: Oid::from(9), ..key(2, 1) };
        let vector = |id| Key::vector(Oid::from(1), Oid::from(2), id);
        let keys =
            [key(1, 1), key(2, 1), key(2, 2), key(2, 3), other_database, vector(2), vector(4)];
        for key in keys {
            drop(published(&mut backend, key));
        }
        backend.unpin();
        let mut asked = Vec::new();
        let pruned = prune(&mut backend, Oid::from(1), Oid::from(2), |kind, ids| {
            let mut ids = ids.to_vec();
            ids.sort_unstable();
            asked.push((kind, ids));
            Ok::<_, ()>(vec![2])
        });
        assert_eq!(
            asked,
            [(Kind::Version, vec![1, 2, 3]), (Kind::Vector, vec![2, 4])],
            "only this installation's versions and vectors are looked up, by kind"
        );
        assert_eq!(pruned, Ok(4));
        let left: Vec<Key> = memory
            .slots()
            .iter()
            .filter(|slot| slot.state != State::Empty)
            .map(|slot| slot.key)
            .collect();
        assert_eq!(left.len(), 3);
        assert!(left.contains(&key(2, 2)) && left.contains(&other_database));
        assert!(left.contains(&vector(2)), "vector 2 exists; version 2's key is another's");
    }

    #[test]
    fn a_prune_with_nothing_shared_reads_nothing() {
        let memory = Memory::new(2, 100, u64::MAX, Duration::ZERO);
        let pruned =
            prune(&mut memory.backend(), Oid::from(1), Oid::from(2), |_, _| -> Result<_, ()> {
                panic!("looked up")
            });
        assert_eq!(pruned, Ok(0));
    }
}
