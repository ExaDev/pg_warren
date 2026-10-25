//! `warren.orphans()` and `warren.drop_orphans()`: the sweep for what
//! neither `drop_owned` nor the event trigger saw, a `DROP ROLE` without
//! `DROP OWNED` (DESIGN.md §5.7, §14 item 45).

use pgrx::prelude::*;

use super::super::CatalogError;
use crate::access::{as_catalog_owner, entry, is_owner, may_drop_orphans, require_orphan_dropper};
use crate::read_only::writing;

/// Every reference to a dropped role, as `(kind, graph, role, owner)`:
/// graphs whose owner was dropped, quotas on readers' vectors, reader
/// entries, and (once per graph and role) vectors whose creator was.
const ORPHANS: &str = "
    SELECT 'graph', g.name, g.owner::oid, g.owner::oid FROM warren.graphs g
    WHERE NOT EXISTS (SELECT FROM pg_catalog.pg_roles r WHERE r.oid = g.owner::oid)
    UNION ALL
    SELECT 'quota', g.name, q.role::oid, g.owner::oid
    FROM warren.vector_quotas q JOIN warren.graphs g USING (graph_id)
    WHERE NOT EXISTS (SELECT FROM pg_catalog.pg_roles r WHERE r.oid = q.role::oid)
    UNION ALL
    SELECT DISTINCT 'reader', g.name, x.reader::oid, g.owner::oid
    FROM warren.graphs g, pg_catalog.unnest(g.readers) AS x (reader)
    WHERE NOT EXISTS (SELECT FROM pg_catalog.pg_roles r WHERE r.oid = x.reader::oid)
    UNION ALL
    SELECT DISTINCT 'vector', g.name, v.created_by::oid, g.owner::oid
    FROM warren.vectors v JOIN warren.graphs g USING (graph_id)
    WHERE NOT EXISTS (SELECT FROM pg_catalog.pg_roles r WHERE r.oid = v.created_by::oid)
    ORDER BY 1, 2, 3";

/// `warren.orphans()`: what names a dropped role (`kind` `graph`,
/// `quota`, `reader` or `vector`), with the graph and the dropped role's oid. A superuser
/// and members of the extension's owner see every row; others see those
/// of graphs they own. `PARALLEL SAFE`: it reads no graph.
#[pg_extern(schema = "warren", volatile, parallel_safe)]
fn orphans(
) -> TableIterator<'static, (name!(kind, String), name!(graph, String), name!(role, pg_sys::Oid))> {
    entry(|_| Ok::<_, CatalogError>(TableIterator::new(visible_orphans()?)))
}

/// The rows of [`ORPHANS`] the caller may see.
fn visible_orphans() -> Result<Vec<(String, String, pg_sys::Oid)>, CatalogError> {
    let everything = may_drop_orphans()?;
    let found = as_catalog_owner(|| {
        Ok::<_, CatalogError>(Spi::connect(|client| {
            client
                .select(ORPHANS, None, &[])?
                .map(|row| {
                    Ok((
                        row.get::<String>(1)?.unwrap_or_default(),
                        row.get::<String>(2)?.unwrap_or_default(),
                        row.get::<pg_sys::Oid>(3)?.unwrap_or(pg_sys::Oid::INVALID),
                        row.get::<pg_sys::Oid>(4)?.unwrap_or(pg_sys::Oid::INVALID),
                    ))
                })
                .collect::<Result<Vec<_>, pgrx::spi::Error>>()
        })?)
    })?;
    Ok(found
        .into_iter()
        .filter(|(_, _, _, owner)| everything || is_owner(*owner))
        .map(|(kind, graph, role, _)| (kind, graph, role))
        .collect())
}

/// `warren.drop_orphans()`: drops every graph whose owner was dropped (as
/// `drop_graph` does), removes reader entries and quotas that name dropped
/// roles and deletes the vectors whose creator was dropped. For a superuser or a
/// member of the extension's owner (DESIGN.md §14 item 45). Returns the
/// number of graphs dropped.
#[pg_extern(schema = "warren")]
fn drop_orphans() -> i64 {
    entry(|_| sweep())
}

/// The condition that no role has the oid in `$column`.
macro_rules! dropped {
    ($column:literal) => {
        concat!("NOT EXISTS (SELECT FROM pg_catalog.pg_roles r WHERE r.oid = ", $column, "::oid)")
    };
}

/// Each pass of the sweep: a query that locks the rows naming a dropped
/// role and returns their keys, then the statement that changes those rows
/// (`$1`, their keys), checking each again.
const PASSES: [(&str, &str); 4] = [
    (
        concat!(
            "SELECT graph_id::int8 FROM warren.graphs g WHERE ",
            dropped!("g.owner"),
            " FOR UPDATE"
        ),
        concat!(
            "DELETE FROM warren.graphs g WHERE graph_id = ANY ($1) AND ",
            dropped!("g.owner"),
            " RETURNING graph_id"
        ),
    ),
    (
        concat!(
            "SELECT graph_id::int8 FROM warren.graphs g WHERE EXISTS (
                     SELECT FROM pg_catalog.unnest(g.readers) AS x (reader)
                     WHERE ",
            dropped!("x.reader"),
            ") FOR UPDATE"
        ),
        concat!(
            "UPDATE warren.graphs SET readers = ARRAY(
                     SELECT x.reader
                     FROM pg_catalog.unnest(readers) WITH ORDINALITY AS x (reader, position)
                     WHERE NOT ",
            dropped!("x.reader"),
            " ORDER BY x.position)::regrole[]
                 WHERE graph_id = ANY ($1)"
        ),
    ),
    (
        concat!(
            "SELECT vector_id FROM warren.vectors v WHERE ",
            dropped!("v.created_by"),
            " FOR UPDATE"
        ),
        concat!(
            "DELETE FROM warren.vectors v WHERE vector_id = ANY ($1) AND ",
            dropped!("v.created_by")
        ),
    ),
    (
        concat!(
            "SELECT graph_id::int8 FROM warren.vector_quotas q WHERE ",
            dropped!("q.role"),
            " FOR UPDATE"
        ),
        concat!(
            "DELETE FROM warren.vector_quotas q WHERE graph_id = ANY ($1) AND ",
            dropped!("q.role")
        ),
    ),
];

fn sweep() -> Result<i64, CatalogError> {
    require_orphan_dropper()?;
    // Each pass locks its rows, then changes them in a statement of its
    // own, whose snapshot follows the lock: a statement that waited on a
    // row would re-check it (EvalPlanQual) under its first snapshot, which
    // misses a role created by the transaction it waited for, and so would
    // drop a graph adopted for that role, or prune a reader just granted.
    let dropped = as_catalog_owner(|| {
        writing(|client| {
            let mut changed = Vec::with_capacity(PASSES.len());
            for (lock, change) in PASSES {
                let keys = client
                    .update(lock, None, &[])?
                    .map(|row| Ok(row.get::<i64>(1)?.unwrap_or_default()))
                    .collect::<Result<Vec<i64>, pgrx::spi::Error>>()?;
                changed.push(match keys.is_empty() {
                    true => 0,
                    false => client.update(change, None, &[keys.into()])?.len(),
                });
            }
            Ok::<_, CatalogError>(changed[0])
        })
    })?;
    i64::try_from(dropped).map_err(|_| CatalogError::TooLarge("graphs dropped"))
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure_as, neighbouring_graphs, ownership, quotas, rows};

    /// [`neighbouring_graphs`], then a bare `DROP ROLE dr_owner`; returns
    /// its OID.
    fn orphaned() -> Result<u32, Error> {
        neighbouring_graphs()?;
        let gone = Spi::get_one::<pg_sys::Oid>("SELECT 'dr_owner'::regrole::oid")?
            .unwrap_or(pg_sys::Oid::INVALID);
        Spi::run("REVOKE ALL ON dr_nodes, dr_edges FROM dr_owner; DROP ROLE dr_owner")?;
        Ok(gone.to_u32())
    }

    const LISTED: &str = "SELECT kind || ':' || graph || ':' || role::text FROM warren.orphans()";

    #[pg_test]
    fn orphans_lists_graphs_quotas_reader_entries_and_vectors_of_dropped_roles() -> Result<(), Error>
    {
        let gone = orphaned()?;
        assert_eq!(
            rows(LISTED)?,
            Some(format!("graph:dr:{gone} quota:dk:{gone} reader:dk:{gone} vector:dk:{gone}"))
        );
        Ok(())
    }

    /// Others see only what dangles from graphs they own; an orphaned
    /// graph is the extension owner's (and a superuser's) to see.
    #[pg_test]
    fn orphans_shows_others_only_what_their_own_graphs_hold() -> Result<(), Error> {
        let gone = orphaned()?;
        Spi::run("CREATE ROLE dk_stranger")?;
        Spi::run("SET ROLE dk_owner")?;
        let owners = rows(LISTED)?;
        Spi::run("SET ROLE dk_stranger")?;
        let strangers = rows(LISTED)?;
        Spi::run("RESET ROLE")?;
        assert_eq!(owners, Some(format!("quota:dk:{gone} reader:dk:{gone} vector:dk:{gone}")));
        assert_eq!(strangers.as_deref(), Some(""));
        Ok(())
    }

    #[pg_test]
    fn drop_orphans_drops_orphaned_graphs_and_prunes_the_rest() -> Result<(), Error> {
        orphaned()?;
        assert_eq!(Spi::get_one::<i64>("SELECT warren.drop_orphans()")?, Some(1));
        assert_eq!(ownership()?.as_deref(), Some("dk:dk_owner:{} / dk.theirs:dk_owner"));
        assert_eq!(quotas()?.as_deref(), Some(""));
        assert_eq!(rows(LISTED)?.as_deref(), Some(""));
        Ok(())
    }

    /// DESIGN.md §14 item 45b, as in a trusted install.
    #[pg_test]
    fn members_of_the_extension_owner_may_drop_orphans_and_others_may_not() -> Result<(), Error> {
        let gone = orphaned()?;
        Spi::run(
            "CREATE ROLE dr_installer; CREATE ROLE dr_member; GRANT dr_installer TO dr_member;
             UPDATE pg_catalog.pg_extension SET extowner = 'dr_installer'::regrole
             WHERE extname = 'pg_warren'",
        )?;
        assert_eq!(
            failure_as("dk_owner", "warren.drop_orphans()")?.as_deref(),
            Some(
                "42501: must be a member of role \"dr_installer\", the owner of extension \
                 pg_warren, to drop graphs whose owner no longer exists"
            )
        );
        Spi::run("SET ROLE dr_member")?;
        let listed = rows(LISTED)?;
        Spi::run("RESET ROLE")?;
        assert_eq!(
            listed,
            Some(format!("graph:dr:{gone} quota:dk:{gone} reader:dk:{gone} vector:dk:{gone}"))
        );
        assert_eq!(failure_as("dr_member", "warren.drop_orphans()")?, None);
        assert_eq!(ownership()?.as_deref(), Some("dk:dk_owner:{} / dk.theirs:dk_owner"));
        Ok(())
    }

    /// PG 16+: a member that neither inherits the extension owner's
    /// privileges nor may `SET ROLE` to it could not `DROP EXTENSION`, so it
    /// may not drop orphans or see them all either (DESIGN.md §14 item 45).
    #[cfg(not(feature = "pg15"))]
    #[pg_test]
    fn a_member_without_inherit_or_set_may_not_drop_orphans() -> Result<(), Error> {
        let gone = orphaned()?;
        Spi::run(
            "CREATE ROLE dr_installer; CREATE ROLE dr_weak; CREATE ROLE dr_setter;
             GRANT dr_installer TO dr_weak WITH INHERIT FALSE, SET FALSE;
             GRANT dr_installer TO dr_setter WITH INHERIT FALSE, SET TRUE;
             UPDATE pg_catalog.pg_extension SET extowner = 'dr_installer'::regrole
             WHERE extname = 'pg_warren'",
        )?;
        assert_eq!(
            failure_as("dr_weak", "warren.drop_orphans()")?.as_deref(),
            Some(
                "42501: must be a member of role \"dr_installer\", the owner of extension \
                 pg_warren, to drop graphs whose owner no longer exists"
            )
        );
        assert_eq!(
            failure_as("dr_weak", "warren.drop_graph('dr')")?.as_deref(),
            Some("42704: the owner of graph \"dr\" no longer exists")
        );
        Spi::run("SET ROLE dr_weak")?;
        let weak = rows(LISTED)?;
        Spi::run("SET ROLE dr_setter")?;
        let setter = rows(LISTED)?;
        Spi::run("RESET ROLE")?;
        assert_eq!(weak.as_deref(), Some(""));
        assert_eq!(
            setter,
            Some(format!("graph:dr:{gone} quota:dk:{gone} reader:dk:{gone} vector:dk:{gone}"))
        );
        assert_eq!(failure_as("dr_setter", "warren.drop_orphans()")?, None);
        Ok(())
    }
}
