//! Opening a version for reading: the access check, which version a call
//! means, and the backend-local cache in front of the loader (DESIGN.md
//! §5.5, §7). Every query function opens its version this way on every
//! call.

use std::sync::Arc;
use std::time::{Duration, Instant};

use pgrx::pg_sys;

use pgrx::notice;
use warren_core::compute::{AnyVector, ComputeMemory};

use super::bound::{admit_load, make_room, unfit, unfit_notice};
use super::shared::{evict_vector, prune as prune_shared, shared, shared_hit};
use super::statement::{notice_due, pin_vector, pin_version, vector_pinned, version_pinned};
use super::versions::{VersionCache, CACHE};
use super::{installation, load, trim, CacheError, CachedIds, LoadedVersion};
use crate::access::{as_catalog_owner, require_reader};
use crate::activity::{self, count_read, Read, Running};
use crate::catalog::{require_version, GraphDefinition, VersionId};
use crate::parallel_worker::cache_notice_raised;
use crate::read_only::{read_one, read_two};
use crate::segment::SpiSegmentStore;
use crate::vector::{
    self, cached_vectors, forget, lookup_vector, remember_written, retain_transients,
    transient_versions, SpiVectorStore, VectorError, VectorId, VectorRecord,
};

/// A graph and the version of it a call reads.
pub(crate) struct Resolved {
    /// The graph, as of the statement's snapshot.
    pub(crate) graph: GraphDefinition,
    /// The version: the one asked for, or the current one.
    pub(crate) version: VersionId,
}

/// Graph `name` and the version a call reads: `version` if given, else its
/// current version; either must be a version of the graph. The caller
/// must be a member of the owner role or of a reader role; `action` names
/// what it does, for the error.
pub(crate) fn resolve(
    name: &str,
    version: Option<VersionId>,
    action: &'static str,
) -> Result<Resolved, CacheError> {
    activity::claim();
    forget_other_installations();
    let graph = GraphDefinition::find(name)?;
    require_reader(graph.owner, &graph.readers, &graph.name, action)?;
    let version = match version {
        Some(version) => {
            require_version(&graph, version)?;
            version
        }
        None => {
            graph.require_sound_current_version()?;
            graph.current_version.ok_or_else(|| CacheError::NoCurrentVersion(graph.name.clone()))?
        }
    };
    Ok(Resolved { graph, version })
}

/// The loaded version `resolved` names, from this backend's cache (the
/// lookup marks it used), from the shared cache (DESIGN.md §5.6; counted
/// by neither of this backend's limits), or loaded into this backend's
/// cache, pinned until the statement ends. A load first prunes cached versions, and transient vectors of
/// versions, that have since been dropped (with their graphs or on their
/// own); it is refused if what the statement then holds in the cache would
/// go past `warren.max_cache_memory` and `warren.max_compute_memory`
/// together; a load of a graph's current version also drops the graph's
/// other cached versions that the statement has not read, which callers
/// reach only by asking for them explicitly (scans holding one keep their
/// copy, and a later explicit call shares or reloads it); and a load ends
/// by trimming the cache to `warren.max_cache_memory`, evicting only what
/// the statement has not read. A hit trims it too, so a lowered cap takes
/// effect at the next lookup.
pub(crate) fn cached<E: CachedIds>(
    resolved: &Resolved,
) -> Result<Arc<LoadedVersion<E>>, CacheError> {
    if let Some(loaded) = CACHE.with_borrow_mut(|cache| cache.get::<E>(resolved.version)) {
        pin_private_hit(resolved.version);
        trim();
        return Ok(loaded);
    }
    if let Some(loaded) = shared::<E>(resolved)? {
        return Ok(loaded);
    }
    load_private(resolved)
}

/// The version `resolved` names, loaded into this backend's cache, as
/// [`cached`] describes.
fn load_private<E: CachedIds>(resolved: &Resolved) -> Result<Arc<LoadedVersion<E>>, CacheError> {
    let (graph_id, version) = (resolved.graph.graph_id, resolved.version);
    prune_dropped()?;
    let expected = expected_bytes::<E>(version)?;
    admit_load(&[version], expected)?;
    if let Some((transient, limit)) = unfit(expected) {
        if cache_notice_raised(notice_due()) {
            notice!("{}", unfit_notice(version, expected, limit, transient));
        }
    }
    make_room(expected);
    // Load outside any borrow of the cache: loading runs SPI, which may
    // raise an ERROR that unwinds through this frame.
    let started = Instant::now();
    let running = Running::start("load", Some(graph_id), Some(version));
    let loaded = load::<E>(&mut SpiSegmentStore, version, resolved.graph.store_merged)?;
    drop(running);
    count_read(Read::PrivateLoad, loaded.bytes(), started.elapsed());
    let current = resolved.graph.current_version == Some(version);
    pin_version(version);
    let loaded = CACHE.with_borrow_mut(|cache| {
        if current {
            cache.evict_graph_except(graph_id, version_pinned);
        }
        cache.insert(graph_id, version, loaded)
    });
    trim();
    Ok(loaded)
}

/// The loaded versions `first` and `second` name, as [`cached`] gives
/// them, both pinned until the statement ends. Those this backend's cache
/// or the shared cache holds are taken (and so pinned) first, so that a
/// load for the other cannot evict them; then those still missing are
/// loaded into the shared cache if it admits them, and the rest are
/// refused together, before either is loaded into this backend's cache,
/// if they would not fit beside what it holds.
pub(crate) fn cached_pair<E: CachedIds>(
    first: &Resolved,
    second: &Resolved,
) -> Result<[Arc<LoadedVersion<E>>; 2], CacheError> {
    let pair = [first, second];
    let mut found = [hit::<E>(first)?, hit::<E>(second)?];
    for (resolved, found) in pair.iter().zip(&mut found) {
        if found.is_none() {
            *found = shared::<E>(resolved)?;
        }
    }
    let mut missing: Vec<VersionId> = pair
        .iter()
        .zip(&found)
        .filter(|(_, found)| found.is_none())
        .map(|(resolved, _)| resolved.version)
        .collect();
    missing.dedup();
    if !missing.is_empty() {
        prune_dropped()?;
        let mut expected = 0u64;
        for &version in &missing {
            expected = expected.saturating_add(expected_bytes::<E>(version)?);
        }
        admit_load(&missing, expected)?;
    }
    let [first_found, second_found] = found;
    let first = match first_found {
        Some(loaded) => loaded,
        None => load_private::<E>(first)?,
    };
    let second = match second_found {
        Some(loaded) => loaded,
        None => load_private::<E>(second)?,
    };
    trim();
    Ok([first, second])
}

/// The loaded version `resolved` names if this backend's cache or the
/// shared cache holds it, pinned until the statement ends; nothing is
/// loaded.
fn hit<E: CachedIds>(resolved: &Resolved) -> Result<Option<Arc<LoadedVersion<E>>>, CacheError> {
    if let Some(loaded) = CACHE.with_borrow_mut(|cache| cache.get::<E>(resolved.version)) {
        pin_private_hit(resolved.version);
        return Ok(Some(loaded));
    }
    shared_hit::<E>(resolved)
}

/// Pins version `version`, found in this backend's cache, for the
/// statement, counting the hit if the statement had not read it yet.
fn pin_private_hit(version: VersionId) {
    if !version_pinned(version) {
        count_read(Read::PrivateHit, 0, Duration::ZERO);
    }
    pin_version(version);
}

/// Persisted vector `id`, if this backend's vector cache holds it: pinned
/// until the statement ends (the lookup marks it used, and the cache is
/// then trimmed). Its bytes are already counted by the cache.
pub(crate) fn cached_vector_hit(id: VectorId) -> Option<Arc<AnyVector>> {
    let vector = lookup_vector(id)?;
    if !vector_pinned(id) {
        count_read(Read::PrivateHit, 0, Duration::ZERO);
    }
    pin_vector(id);
    trim();
    Some(vector)
}

/// Persisted vector `record` of graph `graph_id`, from this backend's
/// vector cache or read into it (the buffer charged to `memory`), pinned
/// until the statement ends; the cache is then trimmed.
pub(crate) fn cached_vector(
    memory: &ComputeMemory,
    graph_id: i32,
    record: &VectorRecord,
) -> Result<Arc<AnyVector>, VectorError> {
    let started = Instant::now();
    let vector = vector::load(&mut SpiVectorStore, memory, graph_id, record)?;
    count_read(Read::PrivateLoad, vector.bytes(), started.elapsed());
    pin_vector(record.vector);
    trim();
    Ok(vector)
}

/// Drops persisted vector `id` from this backend's cache and evicts it
/// from the shared cache (DESIGN.md §14 item 38): its row was just deleted
/// (dropped or replaced) by its creator or a member of its graph's owner
/// role. A deletion that rolls back costs only a reload.
pub(crate) fn forget_vector(id: VectorId) {
    forget(id);
    evict_vector(id);
}

/// Caches persisted vector `vector`, just written as `id` of graph
/// `graph_id`, pinned until the statement ends; the cache is then trimmed.
pub(crate) fn keep_vector(
    graph_id: i32,
    id: VectorId,
    vector: Arc<AnyVector>,
) -> Result<(), VectorError> {
    remember_written(&mut SpiVectorStore, graph_id, id, vector)?;
    pin_vector(id);
    trim();
    Ok(())
}

/// Drops this backend's cached versions (and those evicted that a call
/// still holds), and transient vectors (of every role), of versions that
/// no longer exist as of the statement's snapshot, and evicts such
/// versions from the shared cache (`shared::prune`):
/// dropped with their graphs, by `drop_version` or by retention, here or in
/// another backend, or rolled back. Transient vectors cannot be reloaded,
/// so they are dropped only for deletions that have committed: not in a
/// transaction that has a transaction id, which any transaction that
/// deletes a version has, and whose deletions might still roll back.
/// Cached versions are dropped regardless, since they reload.
pub(super) fn prune_dropped() -> Result<(), CacheError> {
    let mut cached = CACHE.with_borrow(VersionCache::known_versions);
    cached.extend(transient_versions());
    cached.sort_unstable();
    cached.dedup();
    let existing = existing_versions(&cached)?;
    CACHE.with_borrow_mut(|cache| cache.retain(&existing));
    // SAFETY: reads this backend's top-level transaction id, if assigned.
    if unsafe { pg_sys::GetTopTransactionIdIfAny() } == pg_sys::TransactionId::INVALID {
        retain_transients(&existing);
    }
    prune_shared()
}

/// Drops everything this backend's own caches hold (versions, cached
/// vectors and transient vectors) if pg_warren was installed again since it
/// was cached: version, vector and graph ids restart with an installation,
/// so they would name the new one's. Called by `resolve` and `warren.evict`.
pub(super) fn forget_other_installations() {
    if !installation::changed() {
        return;
    }
    CACHE.take();
    for (vector, _, _) in cached_vectors() {
        forget(vector);
    }
    retain_transients(&[]);
}

/// Drops this backend's cached versions of graph `graph_id`; returns how
/// many there were.
pub(crate) fn evict_graph(graph_id: i32) -> usize {
    CACHE.with_borrow_mut(|cache| cache.evict_graph(graph_id))
}

/// About the bytes loading `version` adds to the cache: its stored
/// segments (`warren.versions.bytes`) and what a load builds beside them:
/// a fixed type's second direction (an external id per dense id); nothing
/// for a text id map, which stores both directions (DESIGN.md §5.7).
pub(super) fn expected_bytes<E: CachedIds>(version: VersionId) -> Result<u64, CacheError> {
    let (stored, id_space) = as_catalog_owner(|| {
        Ok::<_, CacheError>(read_two::<i64, i64>(
            "SELECT bytes, id_space FROM warren.versions WHERE version_id = $1",
            &[version.into()],
        )?)
    })?;
    let count = |value: Option<i64>| value.and_then(|value| u64::try_from(value).ok());
    let built = E::built_at_load(count(id_space).unwrap_or(0));
    Ok(count(stored).unwrap_or(0).saturating_add(built))
}

/// Which of `versions` still exist.
fn existing_versions(versions: &[VersionId]) -> Result<Vec<VersionId>, CacheError> {
    if versions.is_empty() {
        return Ok(Vec::new());
    }
    as_catalog_owner(|| {
        Ok::<_, CacheError>(
            read_one::<Vec<VersionId>>(
                "SELECT coalesce(array_agg(version_id), '{}') FROM warren.versions
                 WHERE version_id = ANY ($1)",
                &[versions.to_vec().into()],
            )?
            .unwrap_or_default(),
        )
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;
    use warren_core::graph::FixedKeys;

    use std::sync::Arc;

    use super::{cached, resolve};
    use crate::fixture::{
        failure, refresh, ring, set_current_unchecked, stray_current_failure, two_graphs,
    };

    /// Graph `name`'s current version, through the cache.
    fn open(name: &str) -> Result<Arc<super::LoadedVersion<FixedKeys<i64>>>, Error> {
        let resolved = resolve(name, None, "read").map_err(|_| Error::InvalidPosition)?;
        cached::<FixedKeys<i64>>(&resolved).map_err(|_| Error::InvalidPosition)
    }

    /// A version a call holds, evicted by the cap, is shared by later
    /// lookups even after another graph's load prunes the cache.
    #[pg_test]
    fn a_held_version_is_shared_across_another_graphs_load() -> Result<(), Error> {
        for name in ["held_a", "held_b"] {
            ring(name, 20_000, 1)?;
            refresh(name, true)?;
        }
        Spi::run("SET LOCAL warren.max_cache_memory = '64kB'")?;
        let a = open("held_a")?;
        assert!(Arc::ptr_eq(&a, &open("held_a")?), "shared while held");
        let b = open("held_b")?;
        assert!(Arc::ptr_eq(&a, &open("held_a")?), "still shared after b's load");
        drop(b);
        Ok(())
    }

    /// A restore writes `graphs.current_version` without the triggers'
    /// check (DESIGN.md §5.1), so a call must not read whatever version it
    /// names: another graph's version, or none.
    #[pg_test]
    fn a_current_version_that_is_not_one_of_the_graphs_is_an_error() -> Result<(), Error> {
        let (_, other) = two_graphs("stray")?;
        for stray in [other, -1] {
            set_current_unchecked("stray_a", stray)?;
            assert_eq!(
                failure("warren.degree('stray_a', 1::bigint)")?,
                Some(stray_current_failure("stray_a", stray))
            );
        }
        Ok(())
    }

    /// Behind a transaction pooler, a persisted vector written on one
    /// backend is dropped on another, which cannot tell this backend's
    /// cache; with the shared cache serving reads, this backend loads no
    /// vector, so its next write must prune the dead one (found by the
    /// pooler soak, `test/pooler_soak.py`).
    #[pg_test]
    fn a_written_vector_prunes_the_cached_vectors_dropped_elsewhere() -> Result<(), Error> {
        ring("dropped_elsewhere", 10, 1)?;
        refresh("dropped_elsewhere", true)?;
        Spi::run("SELECT warren.vector_const('dropped_elsewhere', 'a', 1)")?;
        assert_eq!(crate::vector::cached_vector_count(), 1);
        Spi::run("DELETE FROM warren.vectors WHERE name = 'a'")?;
        Spi::run("SELECT warren.vector_const('dropped_elsewhere', 'b', 1)")?;
        assert_eq!(crate::vector::cached_vector_count(), 1, "only b is cached");
        Ok(())
    }
}
