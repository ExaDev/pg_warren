//! Opening a version for reading: the access check, which version a call
//! means, and the backend-local cache in front of the loader (DESIGN.md
//! §5.5, §7). Every query function opens its version this way on every
//! call.

use std::mem::size_of;
use std::sync::Arc;

use pgrx::pg_sys;

use pgrx::notice;
use warren_core::compute::{AnyVector, ComputeMemory};

use super::bound::{admit_load, make_room, unfit, unfit_notice};
use super::statement::{notice_due, pin_vector, pin_version, version_pinned};
use super::versions::{VersionCache, CACHE};
use super::{load, trim, CacheError, LoadedVersion};
use crate::access::{as_catalog_owner, require_reader};
use crate::catalog::{require_version, GraphDefinition, VersionId};
use crate::read_only::{read_one, read_two};
use crate::segment::SpiSegmentStore;
use crate::source::IdDatum;
use crate::vector::{
    self, remember, retain_transients, transient_versions, SpiVectorStore, VectorError, VectorId,
    VectorRecord,
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
/// lookup marks it used) or loaded into it, pinned until the statement
/// ends. A load first prunes cached versions, and transient vectors of
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
pub(crate) fn cached<E: IdDatum + Send + Sync>(
    resolved: &Resolved,
) -> Result<Arc<LoadedVersion<E>>, CacheError> {
    let (graph_id, version) = (resolved.graph.graph_id, resolved.version);
    if let Some(loaded) = CACHE.with_borrow_mut(|cache| cache.get::<E>(version)) {
        pin_version(version);
        trim();
        return Ok(loaded);
    }
    prune_dropped()?;
    let expected = expected_bytes::<E>(version)?;
    admit_load(&[version], expected)?;
    if let Some((transient, limit)) = unfit(expected) {
        if notice_due() {
            notice!("{}", unfit_notice(version, expected, limit, transient));
        }
    }
    make_room(expected);
    // Load outside any borrow of the cache: loading runs SPI, which may
    // raise an ERROR that unwinds through this frame.
    let loaded = load::<E>(&mut SpiSegmentStore, version, resolved.graph.store_merged)?;
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
/// them, both pinned until the statement ends; refused, before either is
/// loaded, if the two would not fit together.
pub(crate) fn cached_pair<E: IdDatum + Send + Sync>(
    first: &Resolved,
    second: &Resolved,
) -> Result<[Arc<LoadedVersion<E>>; 2], CacheError> {
    let mut missing = vec![first.version, second.version];
    missing.dedup();
    missing.retain(|&version| !CACHE.with_borrow(|cache| cache.holds(version)));
    if missing.len() == 2 {
        prune_dropped()?;
        let expected =
            expected_bytes::<E>(missing[0])?.saturating_add(expected_bytes::<E>(missing[1])?);
        admit_load(&missing, expected)?;
    }
    Ok([cached::<E>(first)?, cached::<E>(second)?])
}

/// Persisted vector `record` of graph `graph_id`, from this backend's
/// vector cache or read into it (its bytes charged to `memory`), pinned
/// until the statement ends; the cache is then trimmed.
pub(crate) fn cached_vector(
    memory: &ComputeMemory,
    graph_id: i32,
    record: &VectorRecord,
) -> Result<Arc<AnyVector>, VectorError> {
    let vector = vector::load(&mut SpiVectorStore, memory, graph_id, record)?;
    pin_vector(record.vector);
    trim();
    Ok(vector)
}

/// Caches persisted vector `vector`, just written as `id` of graph
/// `graph_id`, pinned until the statement ends; the cache is then trimmed.
pub(crate) fn keep_vector(graph_id: i32, id: VectorId, vector: Arc<AnyVector>) {
    remember(graph_id, id, vector);
    pin_vector(id);
    trim();
}

/// Drops this backend's cached versions, and transient vectors (of every
/// role), of versions that no longer exist as of the statement's snapshot:
/// dropped with their graphs, by `drop_version` or by retention, here or in
/// another backend, or rolled back. Transient vectors cannot be reloaded,
/// so they are dropped only for deletions that have committed: not in a
/// transaction that has a transaction id, which any transaction that
/// deletes a version has, and whose deletions might still roll back.
/// Cached versions are dropped regardless, since they reload.
pub(super) fn prune_dropped() -> Result<(), CacheError> {
    let mut cached = CACHE.with_borrow(VersionCache::versions);
    cached.extend(transient_versions());
    cached.sort_unstable();
    cached.dedup();
    let existing = existing_versions(&cached)?;
    CACHE.with_borrow_mut(|cache| cache.retain(&existing));
    // SAFETY: reads this backend's top-level transaction id, if assigned.
    if unsafe { pg_sys::GetTopTransactionIdIfAny() } == pg_sys::TransactionId::INVALID {
        retain_transients(&existing);
    }
    Ok(())
}

/// Drops this backend's cached versions of graph `graph_id`; returns how
/// many there were.
pub(crate) fn evict_graph(graph_id: i32) -> usize {
    CACHE.with_borrow_mut(|cache| cache.evict_graph(graph_id))
}

/// About the bytes loading `version` adds to the cache: its stored
/// segments (`warren.versions.bytes`) and the id map's second direction,
/// which is built at load (an external id per dense id).
pub(super) fn expected_bytes<E: IdDatum>(version: VersionId) -> Result<u64, CacheError> {
    let (stored, id_space) = as_catalog_owner(|| {
        Ok::<_, CacheError>(read_two::<i64, i64>(
            "SELECT bytes, id_space FROM warren.versions WHERE version_id = $1",
            &[version.into()],
        )?)
    })?;
    let count = |value: Option<i64>| value.and_then(|value| u64::try_from(value).ok());
    let second_direction = count(id_space).unwrap_or(0).saturating_mul(size_of::<E>() as u64);
    Ok(count(stored).unwrap_or(0).saturating_add(second_direction))
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

    use crate::fixture::{failure, set_current_unchecked, stray_current_failure, two_graphs};

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
}
