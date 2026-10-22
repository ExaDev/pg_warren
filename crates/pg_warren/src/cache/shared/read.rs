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

use pgrx::{ereport, pg_sys, PgSqlErrorCode};

use super::acquire::{acquire, Acquired};
use super::admission::Refusal;
use super::dsm::{store_name, Dsm, DETACH, PREFIX};
use super::setting;
use super::slot::Key;
use super::SharedVersions;
use crate::access::as_catalog_owner;
use crate::cache::load::{fill, plan, LoadedVersion};
use crate::cache::open::Resolved;
use crate::cache::statement::{pin_shared, retire_due, shared_notice_due};
use crate::cache::CacheError;
use crate::catalog::VersionId;
use crate::read_only::read_one;
use crate::segment::SpiSegmentStore;
use crate::source::IdDatum;

/// The version `resolved` names from the shared cache, pinned until the
/// statement ends; `None` to load it privately: the shared cache is off or
/// unavailable, the version was created by this transaction (publishing it
/// would share what no other backend can see, and what a rollback
/// removes), or admission refused it.
pub(crate) fn shared<E: IdDatum + Send + Sync>(
    resolved: &Resolved,
) -> Result<Option<Arc<LoadedVersion<E>>>, CacheError> {
    if setting::cap() == 0 {
        retire();
        return Ok(None);
    }
    let port = match Dsm::open() {
        Ok(port) => port,
        Err(unavailable) => {
            unavailable.notice();
            return Ok(None);
        }
    };
    if created_here(resolved.version)? {
        return Ok(None);
    }
    through(port, resolved)
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
    port.release(DETACH);
}

/// The version `resolved` names through `port`, whatever transaction
/// created it, or `None` if admission refused it.
pub(super) fn through<E: IdDatum + Send + Sync>(
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
    port.release(DETACH);
    pin_shared();
    let (version, store_merged) = (resolved.version, resolved.graph.store_merged);
    let acquired = acquire(
        &mut port,
        key,
        || {
            let plan = plan::<E>(&mut SpiSegmentStore, version, store_merged)?;
            Ok((PREFIX as u64 + plan.len() as u64, plan))
        },
        |plan, region| {
            fill::<E>(&mut SpiSegmentStore, version, plan, region.bytes_mut())?;
            region.validate::<E>(version)
        },
    )?;
    match acquired {
        Acquired::Shared(attached) => Ok(Some(attached.version::<E>(version)?)),
        Acquired::Private(refusal) => {
            let (cap, store) = (setting::cap(), store_name());
            if let Some(message) = refusal_notice(refusal, version, cap, store) {
                if shared_notice_due() {
                    ereport!(NOTICE, PgSqlErrorCode::ERRCODE_SUCCESSFUL_COMPLETION, message);
                }
            }
            Ok(None)
        }
    }
}

/// The registry's key for `version` in this database's installation of
/// pg_warren.
pub(super) fn key(version: VersionId) -> Option<Key> {
    // SAFETY: reads this backend's database and looks pg_warren up in
    // `pg_extension`, on the main thread inside a transaction.
    let (database, extension) =
        unsafe { (pg_sys::MyDatabaseId, pg_sys::get_extension_oid(c"pg_warren".as_ptr(), true)) };
    (extension != pg_sys::Oid::INVALID).then_some(Key { database, extension, version })
}

/// Whether this transaction (or one of its subtransactions) created
/// `version`: its row's `xmin` is one of ours. Read without assigning a
/// transaction id, and only by a transaction that has one, since one that
/// has none has written nothing.
fn created_here(version: VersionId) -> Result<bool, CacheError> {
    // SAFETY: reads this backend's top-level transaction id, if assigned.
    if unsafe { pg_sys::GetTopTransactionIdIfAny() } == pg_sys::TransactionId::INVALID {
        return Ok(false);
    }
    let xmin = as_catalog_owner(|| {
        Ok::<_, CacheError>(read_one::<pg_sys::TransactionId>(
            "SELECT xmin FROM warren.versions WHERE version_id = $1",
            &[version.into()],
        )?)
    })?;
    // SAFETY: asks whether an xid is this transaction's, on the main thread.
    Ok(xmin.is_some_and(|xmin| unsafe { pg_sys::TransactionIdIsCurrentTransactionId(xmin) }))
}

/// The `NOTICE` for a version refused for a reason of configuration
/// (`cap`, the cap in bytes; `store`, where segments are created), or
/// `None` for one that is not (no slot or DSM control slot free now, the
/// cap at 0).
fn refusal_notice(refusal: Refusal, version: VersionId, cap: u64, store: &str) -> Option<String> {
    let private = "so this backend loads its own copy, within warren.max_cache_memory";
    match refusal {
        Refusal::TooLarge => Some(format!(
            "graph version {version} is larger than warren.shared_cache_memory ({cap} bytes), \
             {private}"
        )),
        Refusal::NoRoom => Some(format!(
            "graph version {version} does not fit in warren.shared_cache_memory ({cap} bytes) \
             beside the shared versions in use, {private}"
        )),
        Refusal::StoreFull => Some(format!(
            "graph version {version} does not fit in the free space of {store} for shared \
             memory, {private}; raise the container's --shm-size"
        )),
        Refusal::Disabled | Refusal::NoSlot | Refusal::NoSegment => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_refusals_of_configuration_are_noticed() {
        for refusal in [Refusal::TooLarge, Refusal::NoRoom] {
            let notice = refusal_notice(refusal, 7, 1024, "/dev/shm").unwrap();
            assert!(notice.contains("graph version 7"), "{notice}");
            assert!(notice.contains("warren.shared_cache_memory (1024 bytes)"), "{notice}");
        }
        let store_full = refusal_notice(Refusal::StoreFull, 7, 1024, "/dev/shm").unwrap();
        assert!(store_full.contains("/dev/shm") && store_full.contains("--shm-size"));
        for refusal in [Refusal::Disabled, Refusal::NoSlot, Refusal::NoSegment] {
            assert_eq!(refusal_notice(refusal, 7, 1024, "/dev/shm"), None);
        }
    }
}
