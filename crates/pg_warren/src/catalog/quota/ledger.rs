//! The quota ledger in the catalog: the quota of a role on a graph, what
//! it stores there, and the (graph, role) lock (DESIGN.md §14 item 47).

use pgrx::prelude::*;

use super::super::CatalogError;
use crate::access::as_catalog_owner;
use crate::read_only::read_one_latest;

/// pg_warren's `objsubid` (`locktag_field4`) on its advisory-type locks.
/// Applications' advisory locks have 1 (one `bigint` key) or 2 (two `int`
/// keys), so the quota's lock never collides with theirs; `pg_locks` shows
/// it as `advisory` with this `objsubid`.
pub(crate) const QUOTA_LOCK_OBJSUBID: u16 = 0x5751;

/// The quota of role `$2` on graph `$1`: its own row, else the graph's
/// default (NULL for none).
const QUOTA: &str = "SELECT coalesce(
         (SELECT q.bytes FROM warren.vector_quotas q WHERE q.graph_id = $1 AND q.role::oid = $2),
         (SELECT g.reader_vector_quota FROM warren.graphs g WHERE g.graph_id = $1))";

/// The bytes of the persisted vectors role `$2` wrote on graph `$1`, and of
/// those among them that are vector `$3` (the one replaced, if any), as an
/// array of two: one scan of `vectors (graph_id, created_by)`.
const STORED: &str = concat!(
    "SELECT ARRAY[coalesce(sum(",
    vector_bytes!(),
    "), 0), coalesce(sum(",
    vector_bytes!(),
    ") FILTER (WHERE v.vector_id = $3), 0)]::int8[]
     FROM warren.vectors v WHERE v.graph_id = $1 AND v.created_by::oid = $2"
);

/// The quota of `role` on graph `graph_id`, as committed now.
pub(crate) fn quota_of(graph_id: i32, role: pg_sys::Oid) -> Result<Option<u64>, CatalogError> {
    let quota = as_catalog_owner(|| {
        Ok::<_, CatalogError>(read_one_latest::<i64>(QUOTA, &[graph_id.into(), role.into()])?)
    })?;
    Ok(quota.map(|bytes| u64::try_from(bytes).unwrap_or_default()))
}

/// What `role` stores on graph `graph_id`, and what of it is vector
/// `replacing`, in bytes: under the latest snapshot, which sees what is
/// committed now (whatever the isolation level) and this transaction's own
/// earlier writes.
pub(crate) fn stored(
    graph_id: i32,
    role: pg_sys::Oid,
    replacing: Option<i64>,
) -> Result<(u64, u64), CatalogError> {
    let sums = as_catalog_owner(|| {
        Ok::<_, CatalogError>(read_one_latest::<Vec<i64>>(
            STORED,
            &[graph_id.into(), role.into(), replacing.into()],
        )?)
    })?
    .unwrap_or_default();
    let bytes = |at: usize| sums.get(at).map_or(0, |&sum| u64::try_from(sum).unwrap_or_default());
    Ok((bytes(0), bytes(1)))
}

/// Takes the transaction-scoped lock on (graph `graph_id`, `role`),
/// waiting for a transaction that holds it to end; take it before reading
/// the role's quota or usage. Only on the leader: persisting writers and
/// `reassign_owned` are `PARALLEL UNSAFE`, and a lock a parallel worker
/// took would be released when the worker exits (DESIGN.md §5.7, Spike
/// results).
pub(crate) fn lock(graph_id: i32, role: pg_sys::Oid) {
    // SAFETY: reads the backend's database id, on the main thread.
    let database = unsafe { pg_sys::MyDatabaseId };
    let tag = pg_sys::LOCKTAG {
        locktag_field1: database.to_u32(),
        locktag_field2: graph_id.cast_unsigned(),
        locktag_field3: role.to_u32(),
        locktag_field4: QUOTA_LOCK_OBJSUBID,
        locktag_type: LOCKTAG_ADVISORY,
        locktag_lockmethodid: USER_LOCKMETHOD,
    };
    // SAFETY: a heavyweight lock request on the main thread, held until
    // the transaction ends (not a session lock); waiting, it raises an
    // ERROR on deadlock, `lock_timeout` or cancel, which pgrx's guarded
    // FFI unwinds. With `dontWait` false it always returns having the lock.
    unsafe {
        pg_sys::LockAcquire(&tag, EXCLUSIVE_LOCK, false, false);
    }
}

/// `LOCKTAG_ADVISORY`, as `LOCKTAG.locktag_type` holds it.
const LOCKTAG_ADVISORY: u8 = pg_sys::LockTagType::LOCKTAG_ADVISORY as u8;

/// `USER_LOCKMETHOD`, the lock method of advisory locks.
const USER_LOCKMETHOD: u8 = pg_sys::USER_LOCKMETHOD as u8;

/// `ExclusiveLock`, the mode `pg_advisory_xact_lock` takes.
const EXCLUSIVE_LOCK: pg_sys::LOCKMODE = pg_sys::ExclusiveLock as pg_sys::LOCKMODE;
