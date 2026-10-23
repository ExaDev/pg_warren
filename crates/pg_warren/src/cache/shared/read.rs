//! Reading a version through the shared cache, for `cache::open` (DESIGN.md
//! §5.6): when `warren.shared_cache_memory` is set and the shared cache is
//! available, a version this transaction did not create is looked up in
//! the registry, waited for, or loaded into a new segment and published;
//! otherwise, or when admission refuses it, the caller loads it into the
//! backend's own cache, as in 0.4.0. A refusal whose cause is
//! configuration (the cap, the room for segments) says so in a `NOTICE`
//! once per statement. With the cap set to 0, a read evicts what is
//! shared instead.

use std::sync::Arc;
use std::time::{Duration, Instant};

use pgrx::{ereport, pg_sys, PgSqlErrorCode};

use super::acquire::{acquire, find, Acquired};
use super::admission::Refusal;
use super::dsm::{Dsm, PREFIX};
use super::eviction::{self, evict_shared};
use super::setting;
use super::slot::{Key, Kind};
use super::SharedVersions;
use crate::access::as_catalog_owner;
use crate::activity::{count_read, count_refusal, Read, Refused, Running};
use crate::cache::installation::installation;
use crate::cache::load::{fill, fill_vector, plan, plan_vector, CachedIds, LoadedVersion};
use crate::cache::open::Resolved;
use crate::cache::statement::{pin_shared, retire_due, shared_notice_due};
use crate::cache::CacheError;
use crate::catalog::VersionId;
use crate::parallel_worker::cache_notice_raised;
use crate::read_only::{latest_readable, read_one, read_one_latest};
use crate::segment::SpiSegmentStore;
use crate::vector::{SpiVectorStore, VectorId, VectorRecord};
use warren_core::compute::AnyVector;

/// The version `resolved` names from the shared cache, pinned until the
/// statement ends; `None` to load it privately: the shared cache is off or
/// unavailable, the version was created by this transaction (publishing it
/// would share what no other backend can see, and what a rollback
/// removes), or admission refused it.
pub(crate) fn shared<E: CachedIds>(
    resolved: &Resolved,
) -> Result<Option<Arc<LoadedVersion<E>>>, CacheError> {
    match port_for(resolved)? {
        Ok(port) => through(port, resolved),
        Err(refused) => {
            count_refusal(refused);
            Ok(None)
        }
    }
}

/// The version `resolved` names if the shared cache holds it (waiting
/// while another backend loads it), pinned until the statement ends; `None`
/// where [`shared`] would load privately, and on a miss, which it leaves
/// unclaimed: a two-version call takes what is shared first, so that its
/// partner's load cannot evict it.
pub(crate) fn shared_hit<E: CachedIds>(
    resolved: &Resolved,
) -> Result<Option<Arc<LoadedVersion<E>>>, CacheError> {
    let Ok(mut port) = port_for(resolved)? else {
        return Ok(None);
    };
    let Some(key) = key(resolved.version) else {
        return Ok(None);
    };
    if let Some(attached) = port.pinned(key) {
        return Ok(Some(attached.version::<E>(resolved.version)?));
    }
    port.release();
    pin_shared();
    match find(&mut port, key) {
        Some(attached) => {
            count_read(Read::SharedHit, 0, Duration::ZERO);
            Ok(Some(attached.version::<E>(resolved.version)?))
        }
        None => Ok(None),
    }
}

/// The port to read `resolved` through, or why to load it privately: the
/// shared cache is off or unavailable, or this transaction created the
/// version.
fn port_for(resolved: &Resolved) -> Result<Result<Dsm, Refused>, CacheError> {
    port_unless(|| created_here(VERSION_XMIN, resolved.version))
}

/// The port, or why to read privately: the shared cache is off or
/// unavailable, or `created_here` says this transaction created what is
/// read.
fn port_unless(
    created_here: impl FnOnce() -> Result<bool, CacheError>,
) -> Result<Result<Dsm, Refused>, CacheError> {
    if setting::cap() == 0 {
        retire();
        return Ok(Err(Refused::Unavailable));
    }
    let port = match Dsm::open() {
        Ok(port) => port,
        Err(unavailable) => {
            unavailable.notice();
            return Ok(Err(Refused::Unavailable));
        }
    };
    match created_here()? {
        true => Ok(Err(Refused::Uncommitted)),
        false => Ok(Ok(port)),
    }
}

/// The persisted vector `record` describes from the shared cache, pinned
/// until the statement ends (DESIGN.md §14 item 38); `None` to read it
/// privately: the shared cache is off or unavailable, this transaction
/// wrote it, it is a `bool` vector (which has no region), or admission
/// refused it. A vector is immutable per `vector_id`, so it shares as a
/// version does, and is charged to no call.
pub(crate) fn shared_vector(record: &VectorRecord) -> Result<Option<Arc<AnyVector>>, CacheError> {
    match port_unless(|| created_here(VECTOR_XMIN, record.vector))? {
        Ok(port) => vector_through(port, record),
        Err(refused) => {
            count_refusal(refused);
            Ok(None)
        }
    }
}

/// The persisted vector `record` describes through `port`, whatever
/// transaction wrote it, or `None` if it has no region (a `bool` vector)
/// or admission refused it.
pub(super) fn vector_through(
    mut port: Dsm,
    record: &VectorRecord,
) -> Result<Option<Arc<AnyVector>>, CacheError> {
    let (Some(layout), Some(key)) = (plan_vector(record)?, vector_key(record.vector)) else {
        return Ok(None);
    };
    if let Some(attached) = port.pinned(key) {
        return Ok(Some(attached.vector(record)?));
    }
    port.release();
    pin_shared();
    let mut loaded = Loaded::default();
    let acquired = acquire(
        &mut port,
        key,
        || {
            prune()?;
            Ok::<_, CacheError>((PREFIX as u64 + layout.len() as u64, ()))
        },
        |(), region| {
            loaded.timed(region.bytes_mut().len(), || {
                fill_vector(&mut SpiVectorStore, record, &layout, region.bytes_mut())?;
                Ok::<_, CacheError>(region.validate_vector(record)?)
            })
        },
    )?;
    match acquired {
        Acquired::Shared(attached) => {
            loaded.count();
            Ok(Some(attached.vector(record)?))
        }
        Acquired::Private(refusal) => {
            let what = format!("vector \"{}\" ({})", record.name, record.vector);
            notice_refusal(refusal, &what);
            Ok(None)
        }
    }
}

/// Evicts persisted vector `vector` from the shared cache, its row just
/// deleted by its creator or a member of its graph's owner role (DESIGN.md
/// §14 item 38), and releases this backend's mapping of it. A deletion
/// that rolls back costs only a reload.
pub(crate) fn evict_vector(vector: VectorId) {
    let port = if setting::cap() > 0 { Dsm::open().ok() } else { Dsm::used() };
    if let (Some(mut port), Some(key)) = (port, vector_key(vector)) {
        evict_shared(&mut port, &[key], true);
    }
}

/// `warren.evict`'s share of the shared cache (DESIGN.md §14 item 34):
/// evicts graph `graph`'s unpinned shared versions if `owner` (the caller
/// is a member of its owner role), and releases this backend's mappings of
/// them either way; how many it evicted. The graph's versions are those
/// the statement's snapshot sees; a prune evicts the others.
pub(crate) fn evict_graph(graph: i32, owner: bool) -> Result<usize, CacheError> {
    // Only an owner needs the registry, to evict; at a cap of 0 a read
    // evicts every shared version anyway.
    let port = match owner && setting::cap() > 0 {
        true => Dsm::open().ok(),
        false => Dsm::used(),
    };
    let Some(mut port) = port else {
        return Ok(0);
    };
    Ok(evict_shared(&mut port, &graph_keys(graph)?, owner))
}

/// The keys of graph `graph`'s versions and persisted vectors as of the
/// statement's snapshot.
fn graph_keys(graph: i32) -> Result<Vec<Key>, CacheError> {
    let ids = |query: &str| {
        as_catalog_owner(|| {
            Ok::<_, CacheError>(read_one::<Vec<i64>>(query, &[graph.into()])?.unwrap_or_default())
        })
    };
    let versions = ids(
        "SELECT coalesce(array_agg(version_id), '{}') FROM warren.versions WHERE graph_id = $1",
    )?;
    let vectors =
        ids("SELECT coalesce(array_agg(vector_id), '{}') FROM warren.vectors WHERE graph_id = $1")?;
    Ok(versions
        .into_iter()
        .filter_map(key)
        .chain(vectors.into_iter().filter_map(vector_key))
        .collect())
}

/// Before a miss of `resolved` loads it: evicts what no longer exists, and,
/// if it is its graph's current version, the graph's other versions, as
/// the private cache drops them (DESIGN.md §5.5, §5.6); unpinned ones only,
/// so other backends reading one on purpose (a REPEATABLE READ snapshot,
/// an explicit `version :=`) reload it if they need it again. Current
/// means current in the latest committed state: a reader whose snapshot
/// predates a promotion loads the version it sees as current like any
/// other, and never evicts the one promoted since.
fn make_room(resolved: &Resolved) -> Result<(), CacheError> {
    prune()?;
    if !latest_readable() {
        return Ok(());
    }
    let superseded = superseded_by(resolved.graph.graph_id, resolved.version)?;
    if !superseded.is_empty() {
        evict_shared(&mut Dsm::here(), &superseded, true);
    }
    Ok(())
}

/// The keys of graph `graph`'s versions but `version`, if `version` is its
/// current version in the latest committed state; none otherwise.
fn superseded_by(graph: i32, version: VersionId) -> Result<Vec<Key>, CacheError> {
    let versions = as_catalog_owner(|| {
        Ok::<_, CacheError>(
            read_one_latest::<Vec<VersionId>>(
                "SELECT coalesce(array_agg(v.version_id), '{}')
                 FROM warren.graphs AS g JOIN warren.versions AS v USING (graph_id)
                 WHERE g.graph_id = $1 AND g.current_version = $2 AND v.version_id <> $2",
                &[graph.into(), version.into()],
            )?
            .unwrap_or_default(),
        )
    })?;
    Ok(versions.into_iter().filter_map(key).collect())
}

/// With `warren.shared_cache_memory` set to 0 (the setting is reloaded,
/// not restarted), evicts every unpinned shared version and releases this
/// backend's evicted mappings, once per statement, so that the memory comes
/// back as the backends holding them release them. Only in a backend that
/// has used the shared cache: every backend that maps a version has, and a
/// backend that has not need not find (on PG 17 and 18, create) the
/// registry for it.
fn retire() {
    if let Some(port) = Dsm::used() {
        if retire_due() {
            evict_through(port, &|_| true);
        }
    }
}

/// Evicts the unpinned shared versions whose keys `which` selects, then
/// releases this backend's mappings of them that no call holds.
pub(super) fn evict_through(mut port: Dsm, which: &dyn Fn(&Key) -> bool) {
    port.evict(which);
    port.release();
}

/// The version `resolved` names through `port`, whatever transaction
/// created it, or `None` if admission refused it.
pub(super) fn through<E: CachedIds>(
    mut port: Dsm,
    resolved: &Resolved,
) -> Result<Option<Arc<LoadedVersion<E>>>, CacheError> {
    let Some(key) = key(resolved.version) else {
        return Ok(None);
    };
    // Read again in its statement: no lookup, pin or release.
    if let Some(attached) = port.pinned(key) {
        return Ok(Some(attached.version::<E>(resolved.version)?));
    }
    // What the end of a transaction left to release (a `CALL` that
    // commits, a background worker), before this read takes its lock.
    port.release();
    pin_shared();
    let (version, store_merged) = (resolved.version, resolved.graph.store_merged);
    let mut loaded = Loaded::default();
    let acquired = acquire(
        &mut port,
        key,
        || {
            make_room(resolved)?;
            let plan = plan::<E>(&mut SpiSegmentStore, version, store_merged)?;
            Ok((PREFIX as u64 + plan.len() as u64, plan))
        },
        |plan, region| {
            loaded.timed(region.bytes_mut().len(), || {
                let _running = Running::start("load", Some(resolved.graph.graph_id), Some(version));
                fill::<E>(&mut SpiSegmentStore, version, plan, region.bytes_mut())?;
                Ok::<_, CacheError>(region.validate::<E>(version)?)
            })
        },
    )?;
    match acquired {
        Acquired::Shared(attached) => {
            loaded.count();
            Ok(Some(attached.version::<E>(version)?))
        }
        Acquired::Private(refusal) => {
            notice_refusal(refusal, &format!("graph version {version}"));
            Ok(None)
        }
    }
}

/// Whether a read through the shared cache loaded what it read, and the
/// bytes and time of the load, to count it once the read is done.
#[derive(Default)]
struct Loaded(Option<(u64, Duration)>);

impl Loaded {
    /// Runs `load`, a load of `bytes` into a claimed region, timing it.
    fn timed<R>(&mut self, bytes: usize, load: impl FnOnce() -> R) -> R {
        let started = Instant::now();
        let result = load();
        self.0 = Some((bytes as u64, started.elapsed()));
        result
    }

    /// Counts the read: a shared load if it loaded, else a shared hit.
    fn count(&self) {
        match self.0 {
            Some((bytes, took)) => count_read(Read::SharedLoad, bytes, took),
            None => count_read(Read::SharedHit, 0, Duration::ZERO),
        }
    }
}

/// Says why the shared cache refused `what`, once per statement, if the
/// cause is configuration, and counts the refusal.
fn notice_refusal(refusal: Refusal, what: &str) {
    count_refusal(refusal.reason());
    let (cap, store) = (setting::cap(), crate::shared_memory::store_name());
    if let Some(message) = refusal_notice(refusal, what, cap, store, setting::at_default()) {
        if cache_notice_raised(shared_notice_due()) {
            ereport!(NOTICE, PgSqlErrorCode::ERRCODE_SUCCESSFUL_COMPLETION, message);
        }
    }
}

/// The registry's key for `version` in this database's installation of
/// pg_warren.
pub(super) fn key(version: VersionId) -> Option<Key> {
    let (database, extension) = installation()?;
    Some(Key::version(database, extension, version))
}

/// The registry's key for persisted vector `vector` in this database's
/// installation of pg_warren.
pub(super) fn vector_key(vector: VectorId) -> Option<Key> {
    let (database, extension) = installation()?;
    Some(Key::vector(database, extension, vector))
}

/// Evicts the shared versions and persisted vectors of this database that
/// no longer exist (dropped with their graphs, by `drop_version` or by
/// retention, a vector replaced or dropped, or with an earlier installation
/// of pg_warren), and releases this backend's mappings of them (DESIGN.md
/// §5.6): at a cache miss, private or shared, and at `warren.evict`. Only in a backend that has used the shared
/// cache, and not in parallel mode, which cannot read the latest committed
/// state. Existence is judged by that state, not by the caller's snapshot:
/// a slot only ever holds a committed version (its creator reads it
/// privately until it commits), so one the latest snapshot does not see
/// was deleted, and the deletion committed; an older snapshot would see
/// none of the versions committed since it was taken. Eviction is always
/// safe: a version is immutable, so a snapshot that still sees one
/// reloads it.
pub(crate) fn prune() -> Result<(), CacheError> {
    let (Some(mut port), Some((database, extension))) = (Dsm::used(), installation()) else {
        return Ok(());
    };
    if latest_readable() {
        eviction::prune(&mut port, database, extension, committed)?;
    }
    Ok(())
}

/// Which of `ids`, of versions or vectors as `kind` says, exist in the
/// latest committed state.
fn committed(kind: Kind, ids: &[i64]) -> Result<Vec<i64>, CacheError> {
    let query = match kind {
        Kind::Version => {
            "SELECT coalesce(array_agg(version_id), '{}') FROM warren.versions
             WHERE version_id = ANY ($1)"
        }
        Kind::Vector => {
            "SELECT coalesce(array_agg(vector_id), '{}') FROM warren.vectors
             WHERE vector_id = ANY ($1)"
        }
    };
    as_catalog_owner(|| {
        Ok::<_, CacheError>(
            read_one_latest::<Vec<i64>>(query, &[ids.to_vec().into()])?.unwrap_or_default(),
        )
    })
}

/// A version's row's `xmin`, by `version_id`.
const VERSION_XMIN: &str = "SELECT xmin FROM warren.versions WHERE version_id = $1";

/// A vector's row's `xmin`, by `vector_id`.
const VECTOR_XMIN: &str = "SELECT xmin FROM warren.vectors WHERE vector_id = $1";

/// Whether this transaction (or one of its subtransactions) created the
/// row `xmin_of` reads the `xmin` of, by `id`: its `xmin` is one of ours.
/// Read without assigning a transaction id, and only by a transaction that
/// has one, since one that has none has written nothing.
fn created_here(xmin_of: &str, id: i64) -> Result<bool, CacheError> {
    // SAFETY: reads this backend's top-level transaction id, if assigned.
    if unsafe { pg_sys::GetTopTransactionIdIfAny() } == pg_sys::TransactionId::INVALID {
        return Ok(false);
    }
    let xmin = as_catalog_owner(|| {
        Ok::<_, CacheError>(read_one::<pg_sys::TransactionId>(xmin_of, &[id.into()])?)
    })?;
    // SAFETY: asks whether an xid is this transaction's, on the main thread.
    Ok(xmin.is_some_and(|xmin| unsafe { pg_sys::TransactionIdIsCurrentTransactionId(xmin) }))
}

/// The `NOTICE` for `what` (a graph version or a vector) refused for a
/// reason of configuration (`cap`, the cap in bytes; `store`, where
/// segments are created), or `None` for one that is not (no slot or DSM
/// control slot free now, the cap at 0). A short store is noticed only
/// where the cap was set by hand, not `at_default`: a container's
/// `/dev/shm` would otherwise speak in every statement of a cluster whose
/// owner configured nothing (§14 item 32; `warren.shared_cache()` shows
/// what is shared).
fn refusal_notice(
    refusal: Refusal,
    what: &str,
    cap: u64,
    store: &str,
    at_default: bool,
) -> Option<String> {
    let private = "so this backend loads its own copy, within warren.max_cache_memory";
    match refusal {
        Refusal::TooLarge => Some(format!(
            "{what} is larger than warren.shared_cache_memory ({cap} bytes), {private}"
        )),
        Refusal::NoRoom => Some(format!(
            "{what} does not fit in warren.shared_cache_memory ({cap} bytes) beside the shared \
             versions and vectors in use, {private}"
        )),
        Refusal::StoreFull if at_default => None,
        Refusal::StoreFull => Some(format!(
            "{what} does not fit in the free space of {store} for shared memory, {private}; \
             raise the container's --shm-size"
        )),
        Refusal::Disabled | Refusal::NoSlot | Refusal::NoSegment => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_refusals_of_configuration_are_noticed() {
        let notice = |refusal, at_default| {
            refusal_notice(refusal, "graph version 7", 1024, "/dev/shm", at_default)
        };
        for refusal in [Refusal::TooLarge, Refusal::NoRoom] {
            for at_default in [true, false] {
                let notice = notice(refusal, at_default).unwrap();
                assert!(notice.starts_with("graph version 7 "), "{notice}");
                assert!(notice.contains("warren.shared_cache_memory (1024 bytes)"), "{notice}");
            }
        }
        let store_full =
            refusal_notice(Refusal::StoreFull, "vector \"x\" (3)", 1024, "/dev/shm", false)
                .unwrap();
        assert!(store_full.starts_with("vector \"x\" (3) "), "{store_full}");
        assert!(store_full.contains("/dev/shm") && store_full.contains("--shm-size"));
        for refusal in [Refusal::Disabled, Refusal::NoSlot, Refusal::NoSegment] {
            assert_eq!(notice(refusal, false), None);
        }
    }

    #[test]
    fn a_short_store_is_noticed_only_where_the_cap_was_set_by_hand() {
        let store_full = |at_default| {
            refusal_notice(Refusal::StoreFull, "graph version 7", 1024, "/dev/shm", at_default)
        };
        assert_eq!(store_full(true), None, "a container's /dev/shm, nothing configured");
        assert!(store_full(false).is_some());
    }
}
