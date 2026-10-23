//! `warren.reassign_owned`: the SQL counterpart of `REASSIGN OWNED` for
//! graphs and vectors, which no event trigger can see (DESIGN.md §5.7,
//! §14 item 44).

use pgrx::prelude::*;

use super::super::create_graph::{writable_notice, writable_schemas};
use super::super::graph::NEW_OWNER;
use super::super::quota::{admits, lock, quota_of, stored, vector_bytes, Usage};
use super::super::CatalogError;
use crate::access::{
    as_catalog_owner, entry, may_drop_orphans, refuse_bootstrap_superuser, require_can_receive,
    require_privileges_of, role_name,
};

/// `warren.reassign_owned(old regrole, new regrole)`: gives every graph
/// `old` owns to `new`, with `set_owner`'s rules except its source check
/// (`REASSIGN OWNED` checks none), and makes `new` the creator of every
/// vector `old` wrote, within `new`'s quotas on readers' vectors. The
/// caller needs the privileges of `old` and must
/// be able to `SET ROLE` to `new`. Returns the number of graphs moved.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren."reassign_owned"("old" regrole, "new" regrole) RETURNS bigint
STRICT VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'reassign_owned_wrapper';
"#
)]
fn reassign_owned(old: pg_sys::Oid, new: pg_sys::Oid) -> i64 {
    entry(|_| reassign(old, new))
}

/// Makes `$2` the creator of every vector `$1` wrote, and returns, per
/// graph, the bytes moved where they count against `$2`'s quota: unless
/// `$2` or the caller `$3` is a member of the graph's owner (after the
/// graphs moved), or `$4` (the caller may drop orphans).
const MOVE_VECTORS: &str = concat!(
    "WITH moved AS (
         UPDATE warren.vectors v SET created_by = $2::oid::regrole WHERE v.created_by::oid = $1
         RETURNING v.graph_id, ",
    vector_bytes!(),
    " AS bytes)
     SELECT g.graph_id, g.name, sum(m.bytes)::int8
     FROM moved m JOIN warren.graphs g ON g.graph_id = m.graph_id
     WHERE NOT $4
       AND NOT pg_catalog.pg_has_role($2::oid, g.owner::oid, 'MEMBER')
       AND NOT pg_catalog.pg_has_role($3::oid, g.owner::oid, 'MEMBER')
     GROUP BY g.graph_id, g.name
     ORDER BY g.graph_id"
);

/// Vectors moved to another role on one graph whose bytes count against
/// that role's quota there.
struct CountedMove {
    /// The graph.
    graph_id: i32,
    /// Its name.
    graph: String,
    /// The bytes moved.
    bytes: u64,
}

/// Refuses the move with [`CatalogError::QuotaExceeded`] if it takes what
/// `new` stores on the graph past its quota there (DESIGN.md §5.7): with
/// the (graph, `new`) lock held, as a write by `new` would be, and the
/// move already in what the latest snapshot sees.
fn admit_move(counted: &CountedMove, from: &str, new: pg_sys::Oid) -> Result<(), CatalogError> {
    lock(counted.graph_id, new);
    let Some(quota) = quota_of(counted.graph_id, new)? else {
        return Ok(());
    };
    let (stored, _) = stored(counted.graph_id, new, None)?;
    let usage = Usage::of(stored.saturating_sub(counted.bytes), 0, counted.bytes);
    if admits(false, Some(quota), usage) {
        return Ok(());
    }
    Err(CatalogError::QuotaExceeded {
        from: from.to_owned(),
        to: role_name(new).unwrap_or_default(),
        graph: counted.graph.clone(),
        moved: counted.bytes,
        quota,
        before: usage.before,
        after: usage.after,
    })
}

/// What a graph's search_path NOTICE needs: its name, whether it is built
/// from queries, and its `search_path`.
type Moved = (String, bool, String);

fn reassign(old: pg_sys::Oid, new: pg_sys::Oid) -> Result<i64, CatalogError> {
    let from = require_privileges_of(old, "reassign its graphs and vectors")?;
    refuse_bootstrap_superuser(old, "reassign ownership of")?;
    require_can_receive(new, &from)?;
    // One UPDATE takes each row's lock, waiting for a running refresh or
    // set_owner, and re-checks `owner` once it holds it (EvalPlanQual), as
    // `GraphDefinition::lock` re-checks ownership (DESIGN.md §5.7).
    // SAFETY: reads the backend's current user id: the caller.
    let caller = unsafe { pg_sys::GetUserId() };
    let exempt = may_drop_orphans()?;
    let (moved, counted) = as_catalog_owner(|| {
        Spi::connect_mut(|client| {
            let graphs = client
                .update(
                    &format!(
                        "UPDATE warren.graphs SET {NEW_OWNER} WHERE owner::oid = $1
                         RETURNING name, source = 'query', search_path"
                    ),
                    None,
                    &[old.into(), new.into()],
                )?
                .map(|row| {
                    Ok::<Moved, pgrx::spi::Error>((
                        row.get(1)?.unwrap_or_default(),
                        row.get(2)?.unwrap_or_default(),
                        row.get(3)?.unwrap_or_default(),
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let counted = client
                .update(
                    MOVE_VECTORS,
                    None,
                    &[old.into(), new.into(), caller.into(), exempt.into()],
                )?
                .map(|row| {
                    Ok::<CountedMove, pgrx::spi::Error>(CountedMove {
                        graph_id: row.get(1)?.unwrap_or_default(),
                        graph: row.get(2)?.unwrap_or_default(),
                        bytes: row
                            .get::<i64>(3)?
                            .map_or(0, |b| u64::try_from(b).unwrap_or_default()),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, CatalogError>((graphs, counted))
        })
    })?;
    for counted in &counted {
        admit_move(counted, &from, new)?;
    }
    for (graph, built_from_queries, search_path) in &moved {
        if !built_from_queries {
            continue;
        }
        if let Some(schemas) = writable_schemas(search_path, new)? {
            notice!("{}", writable_notice(graph, search_path, &schemas));
        }
    }
    i64::try_from(moved.len()).map_err(|_| CatalogError::TooLarge("graphs reassigned"))
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{
        failure, failure_as, neighbouring_graphs, ownership, refresh, ring, rows,
    };

    /// `REASSIGN OWNED` checks no source, so neither does this: `dr_heir`
    /// cannot read `dr`'s tables. Reader entries stay, as privileges do.
    #[pg_test]
    fn reassign_owned_gives_the_new_role_every_graph_and_vector_of_the_old() -> Result<(), Error> {
        neighbouring_graphs()?;
        Spi::run("CREATE ROLE dr_heir; SELECT warren.grant('dr', 'dr_heir')")?;
        let moved = Spi::get_one::<i64>("SELECT warren.reassign_owned('dr_owner', 'dr_heir')")?;
        assert_eq!(moved, Some(1));
        assert_eq!(
            ownership()?.as_deref(),
            Some("dk:dk_owner:{dr_owner} dr:dr_heir:{} / dk.mine:dr_heir dk.theirs:dk_owner")
        );
        Ok(())
    }

    #[pg_test]
    fn reassign_owned_of_a_role_that_owns_nothing_moves_nothing() -> Result<(), Error> {
        neighbouring_graphs()?;
        Spi::run("CREATE ROLE dr_idle; CREATE ROLE dr_heir")?;
        let before = ownership()?;
        let moved = Spi::get_one::<i64>("SELECT warren.reassign_owned('dr_idle', 'dr_heir')")?;
        assert_eq!(moved, Some(0));
        assert_eq!(ownership()?, before);
        Ok(())
    }

    /// As `REASSIGN OWNED`: the privileges of the old role (a member that
    /// does not inherit them has not), and the right to become the new.
    #[pg_test]
    fn reassign_owned_needs_the_privileges_of_both_roles() -> Result<(), Error> {
        neighbouring_graphs()?;
        Spi::run(
            "CREATE ROLE dr_heir; CREATE ROLE dr_mover NOINHERIT; CREATE ROLE dr_helper;
             GRANT dr_heir TO dr_mover; GRANT dr_owner TO dr_mover, dr_helper",
        )?;
        let before = ownership()?;
        let call = "warren.reassign_owned('dr_owner', 'dr_heir')";
        assert_eq!(
            failure_as("dr_mover", call)?.as_deref(),
            Some("42501: must have the privileges of role \"dr_owner\" to reassign its graphs and vectors")
        );
        assert_eq!(
            failure_as("dr_helper", call)?.as_deref(),
            Some(
                "42501: must be able to SET ROLE \"dr_heir\" to give it the graphs and vectors of \
                 role \"dr_owner\""
            )
        );
        assert_eq!(ownership()?, before);
        Ok(())
    }

    #[pg_test]
    fn reassign_owned_of_a_role_that_does_not_exist_is_refused() -> Result<(), Error> {
        let gone = Spi::get_one::<pg_sys::Oid>(
            "CREATE ROLE dr_gone; CREATE ROLE dr_heir; SELECT 'dr_gone'::regrole::oid",
        )?
        .unwrap_or(pg_sys::Oid::INVALID);
        Spi::run("DROP ROLE dr_gone")?;
        assert_eq!(
            failure(&format!("warren.reassign_owned({}::oid::regrole, 'dr_heir')", gone.to_u32()))?,
            Some(format!("42704: role with OID {} does not exist", gone.to_u32()))
        );
        Ok(())
    }

    /// As `REASSIGN OWNED BY` the bootstrap superuser.
    #[pg_test]
    fn reassign_owned_refuses_the_bootstrap_superuser() -> Result<(), Error> {
        let name = Spi::get_one::<String>("SELECT rolname::text FROM pg_roles WHERE oid = 10")?
            .unwrap_or_default();
        Spi::run("CREATE ROLE dr_heir")?;
        assert_eq!(
            failure("warren.reassign_owned(10::oid::regrole, 'dr_heir')")?,
            Some(format!(
                "2BP01: cannot reassign ownership of objects owned by role \"{name}\" because \
                 they are required by the database system"
            ))
        );
        Ok(())
    }

    /// Graph `rq`, a ring of 100 nodes (an `f64` vector is 800 bytes) with
    /// a default quota of 1000 bytes, where `rq_mallory`, a member of
    /// `rq_group`, is a reader.
    fn quota_and_a_group() -> Result<(), Error> {
        ring("rq", 100, 2)?;
        refresh("rq", true)?;
        Spi::run(
            "CREATE ROLE rq_group; CREATE ROLE rq_mallory; GRANT rq_group TO rq_mallory;
             SELECT warren.grant('rq', 'rq_mallory');
             SELECT warren.set_vector_quota('rq', 1000)",
        )
    }

    fn vectors() -> Result<Option<String>, Error> {
        rows("SELECT name || ':' || created_by::text FROM warren.vectors ORDER BY name")
    }

    /// The security review of PR #59: moving vectors to another role must
    /// not take that role past its quota, or a reader could empty its own
    /// usage and write again without bound.
    #[pg_test]
    fn reassign_owned_cannot_move_vectors_past_the_new_roles_quota() -> Result<(), Error> {
        quota_and_a_group()?;
        let write = |name: &str| format!("warren.vector_const('rq', '{name}', 1)");
        let reassign = "warren.reassign_owned('rq_mallory', 'rq_group')";
        assert_eq!(failure_as("rq_mallory", &write("a"))?, None);
        assert_eq!(failure_as("rq_mallory", reassign)?, None, "rq_group has room for a");
        assert_eq!(failure_as("rq_mallory", &write("b"))?, None);
        assert_eq!(
            failure_as("rq_mallory", reassign)?.as_deref(),
            Some(
                "53400: reassigning the persisted vectors of role \"rq_mallory\" on graph \"rq\" \
                 (800 bytes) would take those of role \"rq_group\" to 1600 bytes, past its quota \
                 of 1000 bytes (it stores 800); drop some of them first, or ask the graph's owner \
                 for a larger quota"
            )
        );
        assert_eq!(vectors()?.as_deref(), Some("a:rq_group b:rq_mallory"), "nothing moved");
        // The graph's owner may move them: it could set the quota anyway.
        Spi::run("GRANT rq_mallory, rq_group TO rq_owner")?;
        assert_eq!(failure_as("rq_owner", reassign)?, None);
        assert_eq!(vectors()?.as_deref(), Some("a:rq_group b:rq_group"));
        Ok(())
    }

    /// A move to a member of the graph's owner role is exempt, as its
    /// writes are.
    #[pg_test]
    fn reassign_owned_to_the_owner_is_not_counted() -> Result<(), Error> {
        quota_and_a_group()?;
        Spi::run("GRANT rq_owner TO rq_group")?;
        assert_eq!(failure_as("rq_mallory", "warren.vector_const('rq', 'a', 1)")?, None);
        assert_eq!(failure_as("rq_mallory", "warren.vector_const('rq', 'b', 1, 'f32')")?, None);
        assert_eq!(
            failure_as("rq_mallory", "warren.reassign_owned('rq_mallory', 'rq_group')")?,
            None
        );
        assert_eq!(vectors()?.as_deref(), Some("a:rq_group b:rq_group"));
        Ok(())
    }
}
