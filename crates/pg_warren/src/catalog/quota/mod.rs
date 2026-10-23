//! The ledger of quotas on readers' vectors (DESIGN.md §5.7, §14 items 46
//! and 47): the quota that applies to a role on a graph, what the role
//! stores there, the (graph, role) lock under which both are read, and the
//! decision. `vector::quota` admits writes with it and
//! `reassign_owned` moves of vectors to another role.

/// The bytes of persisted vector `v`, a row of `warren.vectors`, in SQL:
/// what it holds when loaded, as `compute_sql::vector_bytes` counts it.
macro_rules! vector_bytes {
    () => {
        "v.length * v.width * CASE v.dtype WHEN 'f32' THEN 4 WHEN 'bool' THEN 1 ELSE 8 END"
    };
}
pub(crate) use vector_bytes;

mod decision;
mod ledger;

pub(crate) use decision::{admits, Usage};
#[cfg(any(test, feature = "pg_test"))]
pub(crate) use ledger::QUOTA_LOCK_OBJSUBID;
pub(crate) use ledger::{lock, quota_of, stored};
