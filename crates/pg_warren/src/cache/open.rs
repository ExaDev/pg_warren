//! Opening a version for reading: the access check, which version a call
//! means, and the backend-local cache in front of the loader (DESIGN.md
//! §5.5, §7). Every query function opens its version this way on every
//! call.

use std::sync::Arc;

use pgrx::pg_sys;

use super::bound::make_room;
use super::versions::{VersionCache, CACHE};
use super::{load, trim, CacheError, LoadedVersion};
use crate::access::{as_catalog_owner, require_reader};
use crate::catalog::{require_version, GraphDefinition, VersionId};
use crate::read_only::read_one;
use crate::segment::SpiSegmentStore;
use crate::source::IdDatum;
use crate::vector::{retain_transients, transient_versions};

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
/// lookup marks it used) or loaded into it. A load first prunes cached
/// versions, and transient vectors of versions, that have since been
/// dropped (with their graphs or on their own); a load of a graph's current
/// version also drops the graph's other cached versions, which callers
/// reach only by asking for them explicitly (scans holding one keep their
/// copy, and a later explicit call reloads it); and a load ends by
/// trimming the cache to `warren.max_cache_memory`, which may evict the
/// version just loaded (the caller still holds it). A hit trims it too, so
/// a lowered cap takes effect at the next lookup.
pub(crate) fn cached<E: IdDatum + Send + Sync>(
    resolved: &Resolved,
) -> Result<Arc<LoadedVersion<E>>, CacheError> {
    let (graph_id, version) = (resolved.graph.graph_id, resolved.version);
    if let Some(loaded) = CACHE.with_borrow_mut(|cache| cache.get::<E>(version)) {
        trim();
        return Ok(loaded);
    }
    prune_dropped()?;
    make_room(stored_bytes(version)?);
    // Load outside any borrow of the cache: loading runs SPI, which may
    // raise an ERROR that unwinds through this frame.
    let loaded = load::<E>(&mut SpiSegmentStore, version, resolved.graph.store_merged)?;
    let current = resolved.graph.current_version == Some(version);
    let loaded = CACHE.with_borrow_mut(|cache| {
        if current {
            cache.evict_graph(graph_id);
        }
        cache.insert(graph_id, version, loaded)
    });
    trim();
    Ok(loaded)
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

/// The bytes of `version`'s stored segments (`warren.versions.bytes`): about
/// what loading it adds to the cache.
pub(super) fn stored_bytes(version: VersionId) -> Result<u64, CacheError> {
    as_catalog_owner(|| {
        let bytes = read_one::<i64>(
            "SELECT bytes FROM warren.versions WHERE version_id = $1",
            &[version.into()],
        )?;
        Ok(bytes.and_then(|bytes| u64::try_from(bytes).ok()).unwrap_or(0))
    })
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
