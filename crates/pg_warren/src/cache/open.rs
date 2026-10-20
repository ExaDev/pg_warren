//! Opening a version for reading: the access check, which version a call
//! means, and the backend-local cache in front of the loader (DESIGN.md
//! §5.5, §7). Every query function opens its version this way on every
//! call.

use std::sync::Arc;

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

/// Graph `name` and the version a call reads: `version` if given (which
/// must be a version of the graph), else its current version. The caller
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
    // Load outside any borrow of the cache: loading runs SPI, which may
    // raise an ERROR that unwinds through this frame.
    let loaded = load::<E>(&mut SpiSegmentStore, version)?;
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
/// another backend, or rolled back.
pub(super) fn prune_dropped() -> Result<(), CacheError> {
    let mut cached = CACHE.with_borrow(VersionCache::versions);
    cached.extend(transient_versions());
    cached.sort_unstable();
    cached.dedup();
    let existing = existing_versions(&cached)?;
    CACHE.with_borrow_mut(|cache| cache.retain(&existing));
    retain_transients(&existing);
    Ok(())
}

/// Drops this backend's cached versions of graph `graph_id`; returns how
/// many there were.
pub(crate) fn evict_graph(graph_id: i32) -> usize {
    CACHE.with_borrow_mut(|cache| cache.evict_graph(graph_id))
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
