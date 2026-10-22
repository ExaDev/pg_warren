//! The `Dsm` adapter in one backend (DESIGN.md §5.6, testing). A
//! `#[pg_test]` runs inside one transaction, so every version it builds is
//! uncommitted, which the shared cache loads privately; the tests that
//! share one read it through `read::through`, which skips that check, and
//! evict it at the end. Multi-backend behaviour is in `test/sql/shared_*`.

use std::ffi::CStr;

use pgrx::pg_sys;
use pgrx::spi::Error;

use super::{with_backend, Dsm};
use crate::cache::open::{resolve, Resolved};
use crate::cache::shared::read::key;
use crate::cache::shared::slot::{Key, State};
use crate::cache::shared::{end_message, SharedVersions};
use crate::cache::statement::end_statement;
use crate::fixture::{refresh, ring};

/// Sets `warren.shared_cache_memory` in this backend, as a reload of the
/// configuration file would (it cannot be `SET`).
fn set_cap(value: &CStr) {
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
}

/// Graph `name`, a ring of `nodes`, with a promoted version: its key and
/// how a call resolves it.
fn graph(name: &str, nodes: i64) -> Result<(Key, Resolved), Error> {
    ring(name, nodes, 1)?;
    let version = refresh(name, true)?;
    let resolved = resolve(name, None, "read").map_err(|_| Error::InvalidPosition)?;
    Ok((key(version).ok_or(Error::InvalidPosition)?, resolved))
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

    use super::{forget, graph, port, set_cap, slot};
    use crate::cache::load::load;
    use crate::cache::shared::acquire::acquire;
    use crate::cache::shared::dsm::PREFIX;
    use crate::cache::shared::read::{shared, through};
    use crate::cache::shared::slot::State;
    use crate::cache::shared::{end_message, SharedVersions};
    use crate::cache::statement::end_statement;
    use crate::segment::SpiSegmentStore;

    #[pg_test]
    fn the_shared_cache_is_off_by_default() -> Result<(), Error> {
        let setting = Spi::get_one::<String>("SHOW warren.shared_cache_memory")?;
        assert_eq!(setting.as_deref(), Some("0"));
        let (key, resolved) = graph("off", 50)?;
        assert!(shared::<i64>(&resolved).map_err(|_| Error::InvalidPosition)?.is_none());
        assert_eq!(slot(key), None);
        Ok(())
    }

    #[pg_test]
    fn an_uncommitted_version_is_loaded_privately() -> Result<(), Error> {
        set_cap(c"64MB");
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
        set_cap(c"64MB");
        let (key, resolved) = graph("shared_once", 2000)?;
        let read =
            || through::<i64>(port()?, &resolved).ok().flatten().ok_or(Error::InvalidPosition);
        let first = read()?;
        assert_eq!(slot(key), Some((State::Ready, 1, 1)), "pinned and mapped by its loader");
        let private = load::<i64>(&mut SpiSegmentStore, key.version, true)
            .map_err(|_| Error::InvalidPosition)?;
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
    fn a_version_larger_than_the_cap_is_loaded_privately() -> Result<(), Error> {
        set_cap(c"64kB");
        let (key, resolved) = graph("too_large", 20_000)?;
        let read = through::<i64>(port()?, &resolved).map_err(|_| Error::InvalidPosition)?;
        assert!(read.is_none());
        assert_eq!(slot(key), None, "a refusal changes nothing");
        Ok(())
    }

    #[pg_test]
    fn a_failed_load_gives_its_slot_up_at_the_abort() -> Result<(), Error> {
        set_cap(c"64MB");
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
    fn a_piece_of_a_version_held_past_it_keeps_the_mapping() -> Result<(), Error> {
        set_cap(c"64MB");
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
        set_cap(c"64MB");
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
}
