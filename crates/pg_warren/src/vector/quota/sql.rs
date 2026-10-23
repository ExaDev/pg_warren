//! `warren.set_vector_quota` and `warren.vector_usage` (DESIGN.md §5.7,
//! §14 item 46).

use pgrx::prelude::*;

use super::QuotaError;
use crate::access::{
    as_catalog_owner, entry, is_owner, require_owner, require_reader, role_name, AccessError,
};
use crate::catalog::quota::vector_bytes;
use crate::catalog::GraphDefinition;

/// What `set_vector_quota` does, for its errors.
const ACTION: &str = "set a vector quota on";

/// `warren.set_vector_quota(name, bytes, role)`: sets graph `name`'s
/// quota on readers' persisted vectors to `bytes`: for `role`, or with
/// `role` NULL the default for every role. NULL `bytes` removes it: the
/// graph's default, or the role's own quota, after which the default
/// applies to the role. For members of the owner role. `role` must name
/// a role to set its quota (`'-'`, `InvalidOid`, is never one).
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren."set_vector_quota"(name text, bytes bigint, role regrole DEFAULT NULL)
RETURNS void VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'set_vector_quota_wrapper';
"#
)]
fn set_vector_quota(name: Option<&str>, bytes: Option<i64>, role: Option<pg_sys::Oid>) {
    entry(|_| set_quota(name.ok_or(QuotaError::NullArgument("name"))?, bytes, role));
}

fn set_quota(name: &str, bytes: Option<i64>, role: Option<pg_sys::Oid>) -> Result<(), QuotaError> {
    if let Some(negative) = bytes.filter(|&bytes| bytes < 0) {
        return Err(QuotaError::Negative(negative));
    }
    match role {
        Some(pg_sys::Oid::INVALID) => return Err(QuotaError::InvalidRole),
        // Removing a dropped role's quota is allowed; setting one is not.
        Some(role) if bytes.is_some() && role_name(role).is_none() => {
            return Err(AccessError::RoleNotFound(role.to_u32()).into());
        }
        _ => {}
    }
    let mut graph = GraphDefinition::find(name)?;
    require_owner(graph.owner, &graph.name, ACTION)?;
    // Checks ownership again once it holds the row: a set_owner may have
    // held it (DESIGN.md §5.7).
    graph.lock(ACTION)?;
    let statement = match (role, bytes) {
        (None, _) => "UPDATE warren.graphs SET reader_vector_quota = $3 WHERE graph_id = $1",
        (Some(_), None) => {
            "DELETE FROM warren.vector_quotas WHERE graph_id = $1 AND role::oid = $2"
        }
        (Some(_), Some(_)) => {
            "INSERT INTO warren.vector_quotas (graph_id, role, bytes)
             VALUES ($1, $2::oid::regrole, $3)
             ON CONFLICT (graph_id, role) DO UPDATE SET bytes = excluded.bytes"
        }
    };
    let role = role.unwrap_or(pg_sys::Oid::INVALID);
    as_catalog_owner(|| {
        Spi::connect_mut(|client| {
            client.update(statement, None, &[graph.graph_id.into(), role.into(), bytes.into()])?;
            Ok::<_, QuotaError>(())
        })
    })
}

/// Each role with persisted vectors or a quota of its own on graph `$1`
/// (and role `$2`, if not NULL), as (role, vectors, bytes, quota): the
/// quota that applies to it, NULL for none or for a member of the owner
/// role, which is exempt. Only role `$2`'s row if it is not NULL.
const USAGE: &str = concat!(
    "SELECT r.role,
            (SELECT count(*) FROM warren.vectors v
             WHERE v.graph_id = $1 AND v.created_by::oid = r.role),
            (SELECT coalesce(sum(",
    vector_bytes!(),
    "), 0)::int8 FROM warren.vectors v
             WHERE v.graph_id = $1 AND v.created_by::oid = r.role),
            CASE WHEN pg_catalog.pg_has_role(r.role, g.owner::oid, 'MEMBER') THEN NULL
                 ELSE coalesce(q.bytes, g.reader_vector_quota) END
     FROM (SELECT v.created_by::oid FROM warren.vectors v WHERE v.graph_id = $1
           UNION SELECT q.role::oid FROM warren.vector_quotas q WHERE q.graph_id = $1
           UNION SELECT $2 WHERE $2 IS NOT NULL) AS r (role)
     JOIN warren.graphs g ON g.graph_id = $1
     LEFT JOIN warren.vector_quotas q ON q.graph_id = $1 AND q.role::oid = r.role
     WHERE $2 IS NULL OR r.role = $2
     ORDER BY r.role::regrole::text"
);

/// `warren.vector_usage(name)`: what each role stores as persisted
/// vectors on graph `name`, and the quota that applies to it. Members of
/// the owner role see every role; a reader sees its own row. `PARALLEL
/// SAFE`: like `info`, it reads catalog rows and no vector.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren."vector_usage"(name text)
RETURNS TABLE (role regrole, vectors bigint, bytes bigint, quota bigint)
STRICT STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'vector_usage_wrapper';
"#
)]
fn vector_usage(
    name: &str,
) -> TableIterator<
    'static,
    (name!(role, pg_sys::Oid), name!(vectors, i64), name!(bytes, i64), name!(quota, Option<i64>)),
> {
    entry(|_| Ok::<_, QuotaError>(TableIterator::new(usage(name)?)))
}

/// A row of `vector_usage`: (role, vectors, bytes, quota).
type UsageRow = (pg_sys::Oid, i64, i64, Option<i64>);

fn usage(name: &str) -> Result<Vec<UsageRow>, QuotaError> {
    let graph = GraphDefinition::find(name)?;
    require_reader(graph.owner, &graph.readers, &graph.name, "read")?;
    // SAFETY: reads the backend's current user id: the caller's role.
    let only = (!is_owner(graph.owner)).then(|| unsafe { pg_sys::GetUserId() });
    as_catalog_owner(|| {
        Ok::<_, QuotaError>(Spi::connect(|client| {
            client
                .select(USAGE, None, &[graph.graph_id.into(), only.into()])?
                .map(|row| {
                    Ok((
                        row.get::<pg_sys::Oid>(1)?.unwrap_or(pg_sys::Oid::INVALID),
                        row.get::<i64>(2)?.unwrap_or_default(),
                        row.get::<i64>(3)?.unwrap_or_default(),
                        row.get::<i64>(4)?,
                    ))
                })
                .collect::<Result<Vec<_>, pgrx::spi::Error>>()
        })?)
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::super::enforce::tests::{graph_with_readers, usage, write};
    use crate::fixture::{failure_as, rows};

    #[pg_test]
    fn a_roles_own_quota_overrides_the_default_until_removed() -> Result<(), Error> {
        graph_with_readers(500)?;
        Spi::run("SELECT warren.set_vector_quota('vq', 2000, 'vq_reader')")?;
        assert_eq!(write("vq_reader", "a", "f64")?, None);
        assert_eq!(write("vq_reader", "b", "f64")?, None);
        let other = write("vq_other", "c", "f64")?;
        assert_eq!(other.map(|error| error[..5].to_owned()).as_deref(), Some("53400"));
        // vq_other has neither vectors nor a quota of its own.
        assert_eq!(usage("vq_owner")?.as_deref(), Some("vq_reader:2:1600:2000"));
        Spi::run(
            "SELECT warren.set_vector_quota('vq', NULL, 'vq_reader');
             SELECT warren.set_vector_quota('vq', NULL)",
        )?;
        assert_eq!(usage("vq_owner")?.as_deref(), Some("vq_reader:2:1600:-"));
        assert_eq!(write("vq_other", "c", "f64")?, None, "no quota left");
        Ok(())
    }

    #[pg_test]
    fn a_reader_sees_only_its_own_usage() -> Result<(), Error> {
        graph_with_readers(1000)?;
        assert_eq!(write("vq_other", "c", "f64")?, None);
        assert_eq!(usage("vq_reader")?.as_deref(), Some("vq_reader:0:0:1000"));
        assert_eq!(usage("vq_owner")?.as_deref(), Some("vq_other:1:800:1000"));
        Spi::run("CREATE ROLE vq_stranger")?;
        assert_eq!(
            failure_as("vq_stranger", "warren.vector_usage('vq')")?.as_deref(),
            Some(
                "42501: must be a member of role \"vq_owner\" or of a reader role to read graph \
                 \"vq\""
            )
        );
        Ok(())
    }

    #[pg_test]
    fn only_the_owner_may_set_a_quota_and_never_a_negative_one() -> Result<(), Error> {
        graph_with_readers(1000)?;
        assert_eq!(
            failure_as("vq_reader", "warren.set_vector_quota('vq', 1, 'vq_reader')")?.as_deref(),
            Some("42501: must be a member of role \"vq_owner\" to set a vector quota on graph \"vq\"")
        );
        assert_eq!(
            failure_as("vq_owner", "warren.set_vector_quota('vq', -1)")?.as_deref(),
            Some("22023: a vector quota cannot be negative: -1 bytes")
        );
        assert_eq!(
            failure_as("vq_owner", "warren.set_vector_quota(NULL, 1)")?.as_deref(),
            Some("22004: name must not be NULL")
        );
        assert_eq!(
            failure_as("vq_owner", "warren.set_vector_quota('nothing', 1)")?.as_deref(),
            Some("42704: graph \"nothing\" does not exist")
        );
        assert_eq!(
            failure_as("vq_owner", "warren.set_vector_quota('vq', 1, '-')")?.as_deref(),
            Some(
                "22023: a vector quota is set for a role, not for InvalidOid ('-'); pass NULL \
                 for the graph's default"
            )
        );
        assert_eq!(
            failure_as("vq_owner", "warren.set_vector_quota('vq', 1, 4000000000::oid::regrole)")?
                .as_deref(),
            Some("42704: role with OID 4000000000 does not exist")
        );
        assert_eq!(failure_as("vq_owner", "warren.set_vector_quota('vq', 1, 'vq_reader')")?, None);
        assert_eq!(failure_as("vq_owner", "warren.set_vector_quota('vq', 2, 'vq_reader')")?, None);
        let rows = rows("SELECT role::text || ':' || bytes FROM warren.vector_quotas")?;
        assert_eq!(rows.as_deref(), Some("vq_reader:2"));
        Ok(())
    }
}
