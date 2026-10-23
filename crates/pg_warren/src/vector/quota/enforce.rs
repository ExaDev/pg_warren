//! Enforcement before a persisted write (DESIGN.md §14 item 47): the
//! (graph, role) lock, the role's usage under the latest committed state,
//! and the decision.

use pgrx::prelude::*;

use super::QuotaError;
use crate::access::{is_owner, role_name};
use crate::catalog::quota::{admits, lock, quota_of, stored, Usage};
use crate::catalog::GraphDefinition;
use crate::vector::VectorId;

/// Admits, or refuses with [`QuotaError::Exceeded`], the current role's
/// persisted write of vector `name` (`written` bytes) to `graph`,
/// replacing vector `replacing` (as the writer checked it), before
/// anything is written. Members of the owner role are exempt; for every
/// other writer, quota or none, the (graph, role) lock is taken before the
/// quota and usage are read and held until the transaction ends, so one
/// role's writes to one graph are counted one at a time.
pub(crate) fn admit(
    graph: &GraphDefinition,
    name: &str,
    replacing: Option<VectorId>,
    written: u64,
) -> Result<(), QuotaError> {
    let exempt = is_owner(graph.owner);
    if exempt {
        return Ok(());
    }
    // SAFETY: reads the backend's current user id: the writing role.
    let role = unsafe { pg_sys::GetUserId() };
    // The lock first, quota or none: a quota set while this write is
    // uncommitted then holds the next writer to both.
    lock(graph.graph_id, role);
    let Some(quota) = quota_of(graph.graph_id, role)? else {
        return Ok(());
    };
    let (stored, replaced) = stored(graph.graph_id, role, replacing)?;
    let usage = Usage::of(stored, replaced, written);
    if admits(exempt, Some(quota), usage) {
        return Ok(());
    }
    Err(QuotaError::Exceeded {
        vector: name.to_owned(),
        written,
        role: role_name(role).unwrap_or_default(),
        graph: graph.name.clone(),
        quota,
        before: usage.before,
        after: usage.after,
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
pub(super) mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::catalog::quota::QUOTA_LOCK_OBJSUBID;
    use crate::fixture::{failure_as, refresh, ring, rows};

    /// Graph `vq`, a ring of 100 nodes (so an `f64` vector is 800 bytes, an
    /// `f32` one 400 and a `bool` one 100), with readers `vq_reader` and
    /// `vq_other`, and a default quota of `default` bytes.
    pub(crate) fn graph_with_readers(default: i64) -> Result<(), Error> {
        ring("vq", 100, 2)?;
        refresh("vq", true)?;
        Spi::run(&format!(
            "CREATE ROLE vq_reader; CREATE ROLE vq_other;
             SELECT warren.grant('vq', 'vq_reader'); SELECT warren.grant('vq', 'vq_other');
             SELECT warren.set_vector_quota('vq', {default})"
        ))
    }

    /// Writes constant vector `name` of `dtype` on `vq` as `role`; the
    /// SQLSTATE and message it failed with, or `None`.
    pub(crate) fn write(role: &str, name: &str, dtype: &str) -> Result<Option<String>, Error> {
        failure_as(role, &format!("warren.vector_const('vq', '{name}', 1, '{dtype}')"))
    }

    /// The persisted vectors on `vq`, as `name:created_by:dtype`.
    pub(crate) fn stored() -> Result<Option<String>, Error> {
        rows(
            "SELECT name || ':' || created_by::text || ':' || dtype FROM warren.vectors
             ORDER BY name",
        )
    }

    /// `warren.vector_usage('vq')` as `role`, as `role:vectors:bytes:quota`.
    pub(crate) fn usage(role: &str) -> Result<Option<String>, Error> {
        Spi::run(&format!("SET ROLE {role}"))?;
        let found = rows(
            "SELECT role::text || ':' || vectors || ':' || bytes || ':' || coalesce(quota::text, '-')
             FROM warren.vector_usage('vq')",
        );
        Spi::run("RESET ROLE")?;
        found
    }

    #[pg_test]
    fn a_write_past_the_quota_is_refused_and_changes_nothing() -> Result<(), Error> {
        graph_with_readers(1000)?;
        assert_eq!(write("vq_reader", "a", "f64")?, None);
        assert_eq!(
            write("vq_reader", "b", "f32")?.as_deref(),
            Some(
                "53400: vector \"b\" (400 bytes) would take the persisted vectors of role \
                 \"vq_reader\" on graph \"vq\" to 1200 bytes, past its quota of 1000 bytes (it \
                 stores 800); drop some of its vectors, write it with persist := false, or ask \
                 the graph's owner for a larger quota"
            )
        );
        assert_eq!(stored()?.as_deref(), Some("a:vq_reader:f64"));
        // Another role's vectors are not counted against it, nor transient ones.
        assert_eq!(write("vq_other", "c", "f64")?, None);
        let transient = "warren.vector_const('vq', 't', 1, persist := false)";
        assert_eq!(failure_as("vq_reader", transient)?, None);
        assert_eq!(write("vq_reader", "b", "bool")?, None);
        assert_eq!(usage("vq_reader")?.as_deref(), Some("vq_reader:2:900:1000"));
        Ok(())
    }

    /// The transaction's own earlier writes count: a pg_test is one
    /// transaction, and so is a library call.
    #[pg_test]
    fn writes_earlier_in_the_transaction_are_counted() -> Result<(), Error> {
        graph_with_readers(1000)?;
        Spi::run("SET ROLE vq_reader; SELECT warren.vector_const('vq', 'a', 1, 'f32')")?;
        Spi::run("SELECT warren.vector_const('vq', 'b', 1, 'f32'); RESET ROLE")?;
        assert_eq!(
            write("vq_reader", "c", "f32")?.map(|error| error[..5].to_owned()).as_deref(),
            Some("53400")
        );
        // A vector it dropped no longer counts.
        Spi::run("SET ROLE vq_reader; SELECT warren.drop_vector('vq', 'a'); RESET ROLE")?;
        assert_eq!(write("vq_reader", "c", "f32")?, None);
        // Nor do those an earlier write of the same call made: the second
        // eigenvector (f64, 800 bytes) does not fit beside the first.
        Spi::run("SELECT warren.set_vector_quota('vq', 2000)")?;
        let eigen = "count(*) FROM warren.eigen('vq', 2, 'laplacian', vectors_prefix := 'e', \
                     dtype := 'f64')";
        let refused = failure_as("vq_reader", eigen)?;
        assert_eq!(refused.map(|error| error[..5].to_owned()).as_deref(), Some("53400"));
        Spi::run("SELECT warren.set_vector_quota('vq', 2400)")?;
        assert_eq!(failure_as("vq_reader", eigen)?, None);
        Ok(())
    }

    #[pg_test]
    fn replacing_a_vector_with_one_no_larger_always_works() -> Result<(), Error> {
        graph_with_readers(1000)?;
        assert_eq!(write("vq_reader", "a", "f64")?, None);
        Spi::run("SELECT warren.set_vector_quota('vq', 100)")?;
        assert_eq!(write("vq_reader", "a", "f64")?, None, "as large");
        assert_eq!(write("vq_reader", "a", "f32")?, None, "smaller");
        let larger = write("vq_reader", "a", "f64")?;
        assert_eq!(larger.map(|error| error[..5].to_owned()).as_deref(), Some("53400"));
        assert_eq!(stored()?.as_deref(), Some("a:vq_reader:f32"));
        Ok(())
    }

    /// A vector the owner replaced is the owner's, and no longer counts
    /// against the reader that wrote it before.
    #[pg_test]
    fn a_vector_counts_against_the_role_that_wrote_it() -> Result<(), Error> {
        graph_with_readers(900)?;
        assert_eq!(write("vq_reader", "b", "f64")?, None);
        assert_eq!(write("vq_owner", "b", "f64")?, None);
        assert_eq!(write("vq_reader", "c", "f64")?, None);
        assert_eq!(stored()?.as_deref(), Some("b:vq_owner:f64 c:vq_reader:f64"));
        assert_eq!(usage("vq_reader")?.as_deref(), Some("vq_reader:1:800:900"));
        Ok(())
    }

    #[pg_test]
    fn a_quota_lowered_below_use_blocks_only_growth() -> Result<(), Error> {
        graph_with_readers(2000)?;
        assert_eq!(write("vq_reader", "a", "f64")?, None);
        assert_eq!(write("vq_reader", "b", "f64")?, None);
        Spi::run("SELECT warren.set_vector_quota('vq', 500)")?;
        assert_eq!(stored()?.as_deref(), Some("a:vq_reader:f64 b:vq_reader:f64"), "kept");
        let growth = write("vq_reader", "c", "bool")?;
        assert_eq!(growth.map(|error| error[..5].to_owned()).as_deref(), Some("53400"));
        assert_eq!(write("vq_reader", "b", "f32")?, None, "shrinking");
        assert_eq!(usage("vq_reader")?.as_deref(), Some("vq_reader:2:1200:500"));
        Ok(())
    }

    #[pg_test]
    fn members_of_the_owner_role_are_exempt() -> Result<(), Error> {
        graph_with_readers(0)?;
        Spi::run("CREATE ROLE vq_member; GRANT vq_owner TO vq_member")?;
        assert_eq!(write("vq_owner", "a", "f64")?, None);
        assert_eq!(write("vq_member", "b", "f64")?, None);
        let refused = write("vq_reader", "c", "bool")?;
        assert_eq!(refused.map(|error| error[..5].to_owned()).as_deref(), Some("53400"));
        assert_eq!(
            usage("vq_owner")?.as_deref(),
            Some("vq_member:1:800:- vq_owner:1:800:-"),
            "exempt roles have no quota"
        );
        Ok(())
    }

    /// The (graph, role) lock is an advisory-type lock with pg_warren's own
    /// `objsubid`, held until the transaction ends, which
    /// `pg_advisory_unlock_all` does not release.
    #[pg_test]
    fn a_counted_write_holds_the_graph_and_role_lock() -> Result<(), Error> {
        graph_with_readers(1000)?;
        let held = format!(
            "SELECT count(*) FROM pg_locks
             WHERE locktype = 'advisory' AND objsubid = {QUOTA_LOCK_OBJSUBID}
               AND pid = pg_backend_pid() AND granted
               AND classid = (SELECT graph_id FROM warren.graphs WHERE name = 'vq')::oid
               AND objid = 'vq_reader'::regrole::oid"
        );
        assert_eq!(Spi::get_one::<i64>(&held)?, Some(0));
        assert_eq!(write("vq_reader", "a", "f64")?, None);
        Spi::run("SELECT pg_advisory_unlock_all()")?;
        assert_eq!(Spi::get_one::<i64>(&held)?, Some(1));
        Ok(())
    }

    /// Bytes as `compute_sql::vector_bytes` counts them, per element type
    /// and width.
    #[pg_test]
    fn usage_counts_each_element_types_bytes() -> Result<(), Error> {
        graph_with_readers(100_000)?;
        Spi::run(
            "SET ROLE vq_reader;
             SELECT warren.vector_const('vq', 'f', 1, 'f64');
             SELECT warren.vector_const('vq', 'g', 1, 'f32');
             SELECT warren.vector_const('vq', 'i', 1, 'i64');
             SELECT warren.vector_const('vq', 'b', 1, 'bool');
             RESET ROLE",
        )?;
        assert_eq!(usage("vq_reader")?.as_deref(), Some("vq_reader:4:2100:100000"));
        Ok(())
    }
}
