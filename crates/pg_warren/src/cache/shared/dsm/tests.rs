//! The `Dsm` adapter in one backend (DESIGN.md §5.6, testing). A
//! `#[pg_test]` runs inside one transaction, so every version it builds is
//! uncommitted, which the shared cache loads privately; the tests that
//! share one read it through `read::through`, which skips that check, and
//! evict it at the end. Multi-backend behaviour is in `test/sql/shared_*`.

use std::ffi::CStr;

use pgrx::pg_sys;
use pgrx::spi::{Error, Spi};

use super::{with_backend, Dsm};
use crate::cache::open::{resolve, Resolved};
use crate::cache::shared::read::{key, vector_key};
use crate::cache::shared::slot::{Key, State};
use crate::cache::shared::{end_message, SharedVersions};
use crate::cache::statement::end_statement;
use crate::fixture::{refresh, ring};
use crate::vector::{SpiVectorStore, VectorRecord, VectorStore};

/// Sets `warren.shared_cache_memory` in this backend, as a reload of the
/// configuration file would (it cannot be `SET`), and holds the shared
/// cache's tests apart until this one's transaction ends: their versions
/// are uncommitted, so a prune in another test's backend, whose snapshot
/// does not see them, would evict them.
fn set_cap(value: &CStr) -> Result<(), Error> {
    Spi::run("SELECT pg_catalog.pg_advisory_xact_lock(7234, 1)")?;
    // SAFETY: sets a setting by name on the main thread, in the context and
    // from the source of the configuration file.
    unsafe {
        pg_sys::SetConfigOption(
            c"warren.shared_cache_memory".as_ptr(),
            value.as_ptr(),
            pg_sys::GucContext::PGC_SIGHUP,
            pg_sys::GucSource::PGC_S_FILE,
        );
    }
    Ok(())
}

/// Graph `name`, a ring of `nodes`, with a promoted version: its key and
/// how a call resolves it.
fn graph(name: &str, nodes: i64) -> Result<(Key, Resolved), Error> {
    ring(name, nodes, 1)?;
    let version = refresh(name, true)?;
    let resolved = resolve(name, None, "read").map_err(|_| Error::InvalidPosition)?;
    Ok((key(version).ok_or(Error::InvalidPosition)?, resolved))
}

/// Graph `name`, a ring of `nodes` with a promoted version, and persisted
/// vector `x` of `dtype` bound to it (every element 1): the vector's key
/// and record.
fn vector(name: &str, nodes: i64, dtype: &str) -> Result<(Key, VectorRecord), Error> {
    ring(name, nodes, 1)?;
    let version = refresh(name, true)?;
    Spi::run(&format!("SELECT warren.vector_const('{name}', 'x', 1, '{dtype}')"))?;
    let bound = SpiVectorStore.bound(version).map_err(|_| Error::InvalidPosition)?;
    let record = bound.into_iter().next().ok_or(Error::InvalidPosition)?;
    Ok((vector_key(record.vector).ok_or(Error::InvalidPosition)?, record))
}

/// The state, pins and mappers of the slot holding `key`, if one does.
fn slot(key: Key) -> Option<(State, u32, u32)> {
    with_backend(|backend| {
        backend.locked(|registry, _| {
            let pins = registry.pins();
            registry
                .slots()
                .iter()
                .zip(pins)
                .find(|(slot, _)| slot.key == key && slot.state != State::Empty)
                .map(|(slot, pins)| (slot.state, pins, slot.mappers))
        })
    })
    .flatten()
}

/// The port, available: the tests run with the preload on PG 15 and 16.
fn port() -> Result<Dsm, Error> {
    Dsm::open().map_err(|_| Error::InvalidPosition)
}

/// Ends the statement and evicts `key`'s version, then releases it.
fn forget(key: Key) -> Result<(), Error> {
    end_statement();
    port()?.evict(&|evicted| *evicted == key);
    end_message();
    Ok(())
}

#[pgrx::pg_schema]
mod tests {
    use std::sync::Arc;

    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::{forget, graph, port, set_cap, slot, vector, with_backend};
    use crate::cache::compute_memory;
    use crate::cache::load::load;
    use crate::cache::open::cached;
    use crate::cache::open::{resolve, Resolved};
    use crate::cache::shared::acquire::acquire;
    use crate::cache::shared::admission::Limits;
    use crate::cache::shared::dsm::PREFIX;
    use crate::cache::shared::read::{evict_through, prune, shared, through, vector_through};
    use crate::cache::shared::registry::Claim;
    use crate::cache::shared::slot::Key;
    use crate::cache::shared::slot::Process;
    use crate::cache::shared::slot::State;
    use crate::cache::shared::{end_message, SharedVersions};
    use crate::cache::statement::end_statement;
    use crate::fixture::{refresh, ring};
    use crate::segment::SpiSegmentStore;
    use crate::vector::{read_vector, SpiVectorStore, VectorRecord, VectorStore};
    use warren_core::compute::AnyVector;

    #[pg_test]
    fn the_shared_cache_is_on_by_default() -> Result<(), Error> {
        let default = Spi::get_one::<String>(
            "SELECT boot_val || unit FROM pg_catalog.pg_settings
             WHERE name = 'warren.shared_cache_memory'",
        )?;
        assert_eq!(default.as_deref(), Some("4194304kB"), "4GB (DESIGN.md §14 item 32)");
        Ok(())
    }

    #[pg_test]
    fn a_cap_of_zero_turns_the_shared_cache_off() -> Result<(), Error> {
        set_cap(c"0")?;
        let setting = Spi::get_one::<String>("SHOW warren.shared_cache_memory")?;
        assert_eq!(setting.as_deref(), Some("0"));
        let (key, resolved) = graph("off", 50)?;
        assert!(shared::<i64>(&resolved).map_err(|_| Error::InvalidPosition)?.is_none());
        assert_eq!(slot(key), None);
        Ok(())
    }

    #[pg_test]
    fn an_uncommitted_version_is_loaded_privately() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, resolved) = graph("uncommitted", 50)?;
        assert!(shared::<i64>(&resolved).map_err(|_| Error::InvalidPosition)?.is_none());
        Spi::run("SELECT warren.prewarm('uncommitted')")?;
        assert_eq!(slot(key), None);
        let evicted = Spi::get_one::<i64>("SELECT warren.evict('uncommitted')")?;
        assert_eq!(evicted, Some(1), "this backend's own cache holds it");
        Ok(())
    }

    #[pg_test]
    fn a_version_is_loaded_once_and_shared_by_later_statements() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, resolved) = graph("shared_once", 2000)?;
        let read =
            || through::<i64>(port()?, &resolved).ok().flatten().ok_or(Error::InvalidPosition);
        let first = read()?;
        assert_eq!(slot(key), Some((State::Ready, 1, 1)), "pinned and mapped by its loader");
        let private =
            load::<i64>(&mut SpiSegmentStore, key.id, true).map_err(|_| Error::InvalidPosition)?;
        assert_eq!((&first.id_map, &first.graph), (&private.id_map, &private.graph));
        end_statement();
        assert_eq!(slot(key), Some((State::Ready, 0, 1)), "mapped across statements");
        let second = read()?;
        assert!(Arc::ptr_eq(&first, &second), "the same mapping and loaded version");
        assert_eq!(slot(key), Some((State::Ready, 1, 1)), "pinned once per statement");
        drop((first, second));
        forget(key)?;
        assert_eq!(slot(key), None);
        Ok(())
    }

    #[pg_test]
    fn a_version_read_again_in_its_statement_skips_the_registry() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, resolved) = graph("fast", 100)?;
        let read =
            || through::<i64>(port()?, &resolved).ok().flatten().ok_or(Error::InvalidPosition);
        let first = read()?;
        let clocks = with_backend(|backend| backend.locked(|registry, _| registry.clocks()));
        let again = read()?;
        let after = with_backend(|backend| backend.locked(|registry, _| registry.clocks()));
        assert_eq!(clocks, after, "no lookup, pin or release in the registry");
        assert!(Arc::ptr_eq(&first, &again));
        drop((first, again));
        forget(key)
    }

    #[pg_test]
    fn a_version_larger_than_the_cap_is_loaded_privately() -> Result<(), Error> {
        set_cap(c"64kB")?;
        let (key, resolved) = graph("too_large", 20_000)?;
        let read = through::<i64>(port()?, &resolved).map_err(|_| Error::InvalidPosition)?;
        assert!(read.is_none());
        assert_eq!(slot(key), None, "a refusal changes nothing");
        Ok(())
    }

    #[pg_test]
    fn a_load_whose_loader_is_gone_is_taken_over() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, resolved) = graph("orphaned", 100)?;
        // A claim by a backend no longer running (no live backend has its
        // pid), as one killed outright mid-load would leave it, if the
        // postmaster did not reset shared memory then.
        let gone = Process { pid: i32::MAX, started: 0 };
        let limits = Limits { cap: 64 << 20, store_room: u64::MAX };
        port()?;
        let claimed = with_backend(|backend| {
            backend.locked(|registry, _| registry.claim(key, 4096, gone, limits))
        });
        assert!(matches!(claimed, Some(Claim::Load { .. })));
        assert_eq!(slot(key), Some((State::Loading, 0, 0)));
        let read = through::<i64>(port()?, &resolved).map_err(|_| Error::InvalidPosition)?;
        assert!(read.is_some(), "a waiter took the load over and loaded it");
        assert_eq!(slot(key), Some((State::Ready, 1, 1)));
        drop(read);
        forget(key)
    }

    #[pg_test]
    fn a_failed_load_gives_its_slot_up_at_the_abort() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, _) = graph("aborted", 50)?;
        let sized = || Ok::<_, String>((PREFIX as u64 + 4096, ()));
        let failed = acquire(&mut port()?, key, sized, |_, _| Err("cancelled".to_owned()));
        assert_eq!(failed.err().as_deref(), Some("cancelled"));
        assert_eq!(slot(key), Some((State::Loading, 0, 0)), "claimed until the abort");
        // What the transaction's abort callback does after the ERROR.
        port()?.abort();
        assert_eq!(slot(key), None, "a waiter loads it");
        Ok(())
    }

    #[pg_test]
    fn the_eviction_of_a_read_at_a_cap_of_0_releases_its_mappings() -> Result<(), Error> {
        // What a read with the cap at 0 does to every shared version, here
        // to this test's alone: the tests share the registry.
        set_cap(c"64MB")?;
        let (key, resolved) = graph("retired", 100)?;
        drop(through::<i64>(port()?, &resolved).map_err(|_| Error::InvalidPosition)?);
        end_statement();
        end_message();
        assert_eq!(slot(key), Some((State::Ready, 0, 1)));
        evict_through(port()?, &|evicted| *evicted == key);
        assert_eq!(slot(key), None, "evicted, and released by this backend");
        Ok(())
    }

    #[pg_test]
    fn a_draining_version_read_again_is_revived() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, resolved) = graph("revived", 100)?;
        let read =
            || through::<i64>(port()?, &resolved).ok().flatten().ok_or(Error::InvalidPosition);
        let first = read()?;
        end_statement();
        assert_eq!(port()?.evict(&|evicted| *evicted == key), 1);
        assert_eq!(slot(key), Some((State::Draining, 0, 1)));
        let again = read()?;
        assert!(Arc::ptr_eq(&first, &again), "through the same mapping");
        assert_eq!(slot(key), Some((State::Ready, 1, 1)), "ready again, and pinned");
        drop((first, again));
        forget(key)?;
        assert_eq!(slot(key), None, "evicted again: its segment was pinned again");
        Ok(())
    }

    #[pg_test]
    fn a_piece_of_a_version_held_past_it_keeps_the_mapping() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, resolved) = graph("piece", 100)?;
        let read = through::<i64>(port()?, &resolved).map_err(|_| Error::InvalidPosition)?;
        let piece = read.ok_or(Error::InvalidPosition)?.id_map.clone();
        end_statement();
        assert_eq!(port()?.evict(&|evicted| *evicted == key), 1);
        end_message();
        assert_eq!(slot(key), Some((State::Draining, 0, 1)), "the piece still reads the segment");
        assert_eq!(piece.external_id(99), Some(100), "and reads it");
        drop(piece);
        end_message();
        assert_eq!(slot(key), None, "released once nothing read through it is held");
        Ok(())
    }

    #[pg_test]
    fn an_evicted_version_is_released_at_the_end_of_the_message() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, resolved) = graph("evicted", 100)?;
        drop(through::<i64>(port()?, &resolved).map_err(|_| Error::InvalidPosition)?);
        end_statement();
        end_message();
        assert_eq!(slot(key), Some((State::Ready, 0, 1)), "a ready version stays mapped");
        assert_eq!(port()?.evict(&|evicted| *evicted == key), 1);
        assert_eq!(slot(key), Some((State::Draining, 0, 1)), "drains while mapped");
        end_message();
        assert_eq!(slot(key), None, "its last mapper released it");
        Ok(())
    }

    #[pg_test]
    fn evict_by_an_owner_evicts_the_graphs_unpinned_shared_versions() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, resolved) = graph("evicted_by_owner", 100)?;
        drop(through::<i64>(port()?, &resolved).map_err(|_| Error::InvalidPosition)?);
        let evict = "SELECT warren.evict('evicted_by_owner')";
        assert_eq!(Spi::get_one::<i64>(evict)?, Some(0), "its statement pins it");
        assert_eq!(slot(key), Some((State::Ready, 1, 0)), "pinned, its mapping released");
        end_statement();
        assert_eq!(Spi::get_one::<i64>(evict)?, Some(1), "counted with the private entries");
        assert_eq!(slot(key), None, "evicted, and released by this backend");
        Ok(())
    }

    #[pg_test]
    fn evict_by_another_role_only_releases_its_own_mappings() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, resolved) = graph("evicted_by_stranger", 100)?;
        drop(through::<i64>(port()?, &resolved).map_err(|_| Error::InvalidPosition)?);
        end_statement();
        Spi::run("CREATE ROLE evict_stranger; SET ROLE evict_stranger")?;
        let evicted = Spi::get_one::<i64>("SELECT warren.evict('evicted_by_stranger')")?;
        Spi::run("RESET ROLE")?;
        assert_eq!(evicted, Some(0));
        assert_eq!(slot(key), Some((State::Ready, 0, 0)), "still shared, mapped by no one");
        forget(key)
    }

    /// Graph `name`'s version shared, then superseded by a refresh whose
    /// retention drops it: its key and the new version's resolution.
    fn superseded(name: &str) -> Result<(Key, Resolved), Error> {
        let (key, resolved) = graph(name, 100)?;
        drop(through::<i64>(port()?, &resolved).map_err(|_| Error::InvalidPosition)?);
        end_statement();
        refresh(name, true)?;
        assert_eq!(slot(key), Some((State::Ready, 0, 1)));
        let resolved = resolve(name, None, "read").map_err(|_| Error::InvalidPosition)?;
        Ok((key, resolved))
    }

    #[pg_test]
    fn a_private_load_prunes_shared_versions_that_no_longer_exist() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, _) = superseded("pruned_privately")?;
        Spi::run("SELECT warren.prewarm('pruned_privately')")?;
        assert_eq!(slot(key), None, "evicted, and released by this backend");
        Ok(())
    }

    #[pg_test]
    fn a_shared_load_prunes_shared_versions_that_no_longer_exist() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, resolved) = superseded("pruned_shared")?;
        let current = through::<i64>(port()?, &resolved).map_err(|_| Error::InvalidPosition)?;
        assert!(current.is_some());
        assert_eq!(slot(key), None, "evicted, and released by this backend");
        drop(current);
        forget(super::key(resolved.version).ok_or(Error::InvalidPosition)?)
    }

    /// Graph `name`, a ring that retains two versions, its first version
    /// shared and its second promoted: their keys and resolutions.
    fn promoted(name: &str) -> Result<[(Key, Resolved); 2], Error> {
        ring(name, 100, 2)?;
        let first = refresh(name, true)?;
        refresh(name, true)?;
        let opened = |version| -> Result<(Key, Resolved), Error> {
            let resolved = resolve(name, version, "read").map_err(|_| Error::InvalidPosition)?;
            Ok((super::key(resolved.version).ok_or(Error::InvalidPosition)?, resolved))
        };
        Ok([opened(Some(first))?, opened(None)?])
    }

    #[pg_test]
    fn loading_the_current_version_evicts_the_graphs_other_unpinned_versions() -> Result<(), Error>
    {
        set_cap(c"64MB")?;
        let [(old, old_read), (current, current_read)] = promoted("promoted")?;
        drop(through::<i64>(port()?, &old_read).map_err(|_| Error::InvalidPosition)?);
        end_statement();
        drop(through::<i64>(port()?, &current_read).map_err(|_| Error::InvalidPosition)?);
        assert_eq!(slot(old), None, "evicted, and released by this backend");
        assert_eq!(slot(current), Some((State::Ready, 1, 1)));
        forget(current)
    }

    #[pg_test]
    fn loading_the_current_version_spares_the_versions_a_statement_pins() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let [(old, old_read), (current, current_read)] = promoted("promoted_pinned")?;
        let held = through::<i64>(port()?, &old_read).map_err(|_| Error::InvalidPosition)?;
        drop(through::<i64>(port()?, &current_read).map_err(|_| Error::InvalidPosition)?);
        assert_eq!(slot(old), Some((State::Ready, 1, 1)), "its statement pins it");
        drop(held);
        forget(old)?;
        forget(current)
    }

    #[pg_test]
    fn loading_another_version_evicts_nothing() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let [(old, old_read), (current, current_read)] = promoted("promoted_other")?;
        drop(through::<i64>(port()?, &current_read).map_err(|_| Error::InvalidPosition)?);
        end_statement();
        drop(through::<i64>(port()?, &old_read).map_err(|_| Error::InvalidPosition)?);
        assert_eq!(slot(current), Some((State::Ready, 0, 1)));
        forget(old)?;
        forget(current)
    }

    #[pg_test]
    fn a_shared_version_is_charged_to_no_call_and_reclaimed_by_none() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, resolved) = graph("uncharged", 2000)?;
        Spi::run("SET LOCAL warren.max_cache_memory = '64kB'")?;
        let shared = through::<i64>(port()?, &resolved).map_err(|_| Error::InvalidPosition)?;
        assert!(shared.is_some(), "about 94 kB, larger than the private cap");
        assert_eq!(compute_memory().in_use(), 0, "nothing of the shared version is charged");
        let budget = compute_memory();
        budget.reclaim_for(u64::MAX);
        assert_eq!(slot(key), Some((State::Ready, 1, 1)), "nor evicted by a budget's reclaim");
        // The same version read privately (it is uncommitted) is charged
        // past the cap while the call holds it.
        let private = cached::<i64>(&resolved).map_err(|_| Error::InvalidPosition)?;
        assert!(compute_memory().in_use() > 0, "a private version past the cap is charged");
        drop((shared, private));
        forget(key)
    }

    /// Persisted vector `record` through the shared cache, whatever wrote
    /// it.
    fn shared_vector(record: &VectorRecord) -> Result<Arc<AnyVector>, Error> {
        vector_through(port()?, record).ok().flatten().ok_or(Error::InvalidPosition)
    }

    #[pg_test]
    fn a_persisted_vector_is_loaded_once_and_shared_by_later_statements() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, record) = vector("vector_once", 2000, "f64")?;
        let first = shared_vector(&record)?;
        assert_eq!(slot(key), Some((State::Ready, 1, 1)), "pinned and mapped by its loader");
        let mut store = SpiVectorStore;
        let private = read_vector(&mut store, &compute_memory(), &record)
            .map_err(|_| Error::InvalidPosition)?;
        assert_eq!(*first, private, "the vector as it decodes");
        end_statement();
        assert_eq!(slot(key), Some((State::Ready, 0, 1)), "mapped across statements");
        let second = shared_vector(&record)?;
        assert!(Arc::ptr_eq(&first, &second), "the same mapping and vector");
        assert_eq!(compute_memory().in_use(), 0, "charged to no call");
        let version = super::key(key.id).ok_or(Error::InvalidPosition)?;
        assert_eq!(slot(version), None, "a vector's id is not a version's");
        drop((first, second));
        forget(key)?;
        assert_eq!(slot(key), None);
        Ok(())
    }

    #[pg_test]
    fn an_uncommitted_vector_is_read_privately() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, record) = vector("vector_uncommitted", 50, "f64")?;
        let read = crate::cache::shared_vector(&record).map_err(|_| Error::InvalidPosition)?;
        assert!(read.is_none());
        assert_eq!(slot(key), None);
        Ok(())
    }

    #[pg_test]
    fn a_bool_vector_is_read_privately() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, record) = vector("vector_bool", 50, "bool")?;
        let read = vector_through(port()?, &record).map_err(|_| Error::InvalidPosition)?;
        assert!(read.is_none(), "a bool vector has no region");
        assert_eq!(slot(key), None);
        Ok(())
    }

    #[pg_test]
    fn dropping_a_shared_vector_evicts_it() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, record) = vector("vector_dropped", 100, "i64")?;
        drop(shared_vector(&record)?);
        end_statement();
        Spi::run("SELECT warren.drop_vector('vector_dropped', 'x')")?;
        assert_eq!(slot(key), None, "evicted, and released by this backend");
        Ok(())
    }

    #[pg_test]
    fn replacing_a_shared_vector_evicts_it() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, record) = vector("vector_replaced", 100, "f32")?;
        drop(shared_vector(&record)?);
        end_statement();
        Spi::run("SELECT warren.vector_const('vector_replaced', 'x', 2, 'f32')")?;
        assert_eq!(slot(key), None, "evicted, and released by this backend");
        Ok(())
    }

    #[pg_test]
    fn evict_by_an_owner_evicts_the_graphs_shared_vectors() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, record) = vector("vector_evicted", 100, "f64")?;
        drop(shared_vector(&record)?);
        end_statement();
        let evicted = Spi::get_one::<i64>("SELECT warren.evict('vector_evicted')")?;
        // The version and the vector its uncommitted write cached
        // privately, and the shared vector.
        assert_eq!(evicted, Some(3));
        assert_eq!(slot(key), None, "evicted, and released by this backend");
        Ok(())
    }

    #[pg_test]
    fn a_prune_evicts_shared_vectors_whose_rows_are_gone() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (key, record) = vector("vector_pruned", 100, "f64")?;
        drop(shared_vector(&record)?);
        end_statement();
        // Deleted behind the cache's back, as another backend's drop would.
        Spi::run(&format!("DELETE FROM warren.vectors WHERE vector_id = {}", record.vector))?;
        assert_eq!(slot(key), Some((State::Ready, 0, 1)));
        prune().map_err(|_| Error::InvalidPosition)?;
        assert_eq!(slot(key), None, "evicted, and released by this backend");
        Ok(())
    }

    /// What `warren.shared_cache()` lists of graph `graph`, as kind and
    /// state, and its `cluster` row's bytes.
    fn listed(graph: &str) -> Result<(Option<String>, Option<i64>), Error> {
        let entries = Spi::get_one::<String>(&format!(
            "SELECT string_agg(kind || ':' || state, ',' ORDER BY kind)
             FROM warren.shared_cache() WHERE graph = '{graph}'"
        ))?;
        let cluster = Spi::get_one::<i64>(
            "SELECT bytes FROM warren.shared_cache() WHERE kind = 'cluster' AND cap IS NOT NULL",
        )?;
        Ok((entries, cluster))
    }

    #[pg_test]
    fn the_view_lists_shared_versions_and_vectors_to_their_graphs_readers() -> Result<(), Error> {
        set_cap(c"64MB")?;
        let (version, resolved) = graph("listed", 100)?;
        drop(through::<i64>(port()?, &resolved).map_err(|_| Error::InvalidPosition)?);
        Spi::run("SELECT warren.vector_const('listed', 'x', 1)")?;
        let bound = SpiVectorStore.bound(version.id).map_err(|_| Error::InvalidPosition)?;
        let record = bound.into_iter().next().ok_or(Error::InvalidPosition)?;
        drop(shared_vector(&record)?);
        let (entries, cluster) = listed("listed")?;
        assert_eq!(entries.as_deref(), Some("vector:ready,version:ready"));
        assert!(cluster.is_some_and(|bytes| bytes > 0), "{cluster:?}");
        let pins = Spi::get_one::<i32>(
            "SELECT pins FROM warren.shared_cache() WHERE graph = 'listed' AND kind = 'version'",
        )?;
        assert_eq!(pins, Some(1), "its statement pins it");
        Spi::run("CREATE ROLE view_stranger; SET ROLE view_stranger")?;
        let (hidden, totals) = listed("listed")?;
        Spi::run("RESET ROLE; SELECT warren.grant('listed', 'view_stranger')")?;
        assert_eq!(hidden, None, "a role that cannot read the graph sees none of it");
        assert_eq!(totals, cluster, "but sees the cluster's totals");
        Spi::run("SET ROLE view_stranger")?;
        let (granted, _) = listed("listed")?;
        Spi::run("RESET ROLE")?;
        assert_eq!(granted, entries, "a reader sees it");
        end_statement();
        forget(super::vector_key(record.vector).ok_or(Error::InvalidPosition)?)?;
        forget(version)?;
        assert_eq!(listed("listed")?.0, None, "evicted");
        Ok(())
    }
}
