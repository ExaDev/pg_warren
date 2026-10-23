//! Ownership transfer: `warren.set_owner` gives a graph a new owner, as
//! `ALTER MATERIALIZED VIEW … OWNER TO` does a materialised view (DESIGN.md
//! §5.7, §14 item 43), and `warren.reassign_owned` (`reassign`) every
//! graph and vector of one role to another, as `REASSIGN OWNED` does.

mod reassign;

use pgrx::prelude::*;

use super::create_graph::{writable_notice, writable_schemas};
use super::{CatalogError, GraphDefinition, GraphSource};
use crate::access::{
    as_role, entry, require_adopter, require_can_become, require_owner, with_read_only,
    with_search_path, AccessError,
};
use crate::source::{check_edge_columns, require_node_id_type, source_columns};

/// What `set_owner` does, for its errors.
const ACTION: &str = "change the owner of";

/// `warren.set_owner(name text, owner regrole)`: makes `owner` the owner
/// of graph `name`. The caller must be a member of the current owner and
/// able to `SET ROLE` to the new one (on PG 15, a member of it); only a
/// superuser may adopt a graph whose owner was dropped. Versions, vectors
/// and readers (but the new owner) stay; the old owner keeps no access
/// unless it is granted.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren."set_owner"(name text, owner regrole) RETURNS void
STRICT VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'set_owner_wrapper';
"#
)]
fn set_owner(name: &str, owner: pg_sys::Oid) {
    entry(|_| transfer(name, owner));
}

fn transfer(name: &str, owner: pg_sys::Oid) -> Result<(), CatalogError> {
    let mut graph = GraphDefinition::find(name)?;
    // A graph whose owner was dropped may be adopted by a superuser alone
    // (DESIGN.md §14 item 45).
    let orphan = match require_owner(graph.owner, &graph.name, ACTION) {
        Err(AccessError::OwnerDropped(_)) => require_adopter(&graph.name).map(|()| true)?,
        checked => checked.map(|()| false)?,
    };
    require_can_become(owner, &graph.name)?;
    match graph.lock(ACTION) {
        Err(CatalogError::Access(AccessError::OwnerDropped(_))) if orphan => {}
        locked => locked?,
    }
    let built_from_queries = graph.source == GraphSource::Query;
    if built_from_queries {
        open_sources_as(&graph, owner)?;
    }
    graph.set_owner(owner)?;
    if !built_from_queries {
        return Ok(());
    }
    if let Some(schemas) = writable_schemas(&graph.search_path, owner)? {
        notice!("{}", writable_notice(&graph.name, &graph.search_path, &schemas));
    }
    Ok(())
}

/// Opens (without running) the graph's source queries as `owner`, under
/// the graph's `search_path` and in the read-only state, as `refresh` will
/// run them, and checks their columns: a role that cannot read them is
/// refused with 42501, columns that no longer match with 42804.
fn open_sources_as(graph: &GraphDefinition, owner: pg_sys::Oid) -> Result<(), CatalogError> {
    let (nodes, edges) = as_role(owner, || {
        with_search_path(&graph.search_path, || {
            with_read_only(|| {
                Ok::<_, CatalogError>((
                    source_columns::<CatalogError>("node_query", &graph.node_query)?,
                    source_columns::<CatalogError>("edge_query", &graph.edge_query)?,
                ))
            })
        })
    })?;
    require_node_id_type(&nodes, graph.id_type)?;
    check_edge_columns(&edges, graph.id_type, graph.has_relations, graph.weighted)?;
    Ok(())
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, failure_as as as_role, refresh, ring, versions};

    /// Graph `so`, owned by `so_owner`, refreshed once, and role `so_new`,
    /// which may read its sources.
    fn graph() -> Result<i64, Error> {
        ring("so", 4, 2)?;
        Spi::run("CREATE ROLE so_new; GRANT SELECT ON so_nodes, so_edges TO so_new")?;
        refresh("so", true)
    }

    fn owner() -> Result<Option<String>, Error> {
        Spi::get_one::<String>("SELECT owner::text FROM warren.graphs WHERE name = 'so'")
    }

    #[pg_test]
    fn the_new_owner_refreshes_the_graph_and_the_old_one_may_not() -> Result<(), Error> {
        let built = graph()?;
        Spi::run("SELECT warren.set_owner('so', 'so_new')")?;
        assert_eq!(owner()?.as_deref(), Some("so_new"));
        assert_eq!(versions("so")?, Some(format!("{built}:false")), "versions stay as built");
        assert_eq!(as_role("so_new", "warren.refresh('so')")?, None);
        assert_eq!(
            as_role("so_owner", "warren.refresh('so')")?.as_deref(),
            Some("42501: must be a member of role \"so_new\" to refresh graph \"so\"")
        );
        Ok(())
    }

    #[pg_test]
    fn the_old_owner_may_no_longer_read_the_graph() -> Result<(), Error> {
        graph()?;
        assert_eq!(as_role("so_owner", "warren.degree('so', 1::bigint)")?, None);
        Spi::run("SELECT warren.set_owner('so', 'so_new')")?;
        assert_eq!(
            as_role("so_owner", "warren.degree('so', 1::bigint)")?.as_deref(),
            Some("42501: must be a member of role \"so_new\" or of a reader role to read graph \"so\"")
        );
        Ok(())
    }

    #[pg_test]
    fn readers_are_kept_but_the_new_owner_is_no_longer_one() -> Result<(), Error> {
        graph()?;
        Spi::run(
            "CREATE ROLE so_reader;
             SELECT warren.grant('so', 'so_reader'); SELECT warren.grant('so', 'so_new');
             SELECT warren.set_owner('so', 'so_new')",
        )?;
        let readers =
            Spi::get_one::<String>("SELECT readers::text FROM warren.graphs WHERE name = 'so'")?;
        assert_eq!(readers.as_deref(), Some("{so_reader}"));
        Ok(())
    }

    #[pg_test]
    fn a_new_owner_that_cannot_read_the_sources_is_refused() -> Result<(), Error> {
        graph()?;
        Spi::run("REVOKE SELECT ON so_edges FROM so_new")?;
        assert_eq!(
            failure("warren.set_owner('so', 'so_new')")?.as_deref(),
            Some("42501: permission denied for table so_edges")
        );
        assert_eq!(owner()?.as_deref(), Some("so_owner"));
        Ok(())
    }

    #[pg_test]
    fn sources_whose_columns_no_longer_match_are_refused() -> Result<(), Error> {
        graph()?;
        Spi::run("ALTER TABLE so_nodes ALTER id TYPE int")?;
        assert_eq!(
            failure("warren.set_owner('so', 'so_new')")?.as_deref(),
            Some("42804: node_query column 1 (node id) must be of type bigint, not integer")
        );
        assert_eq!(owner()?.as_deref(), Some("so_owner"));
        Ok(())
    }

    #[pg_test]
    fn the_caller_must_be_a_member_of_the_current_owner() -> Result<(), Error> {
        graph()?;
        Spi::run("CREATE ROLE so_mover; GRANT so_new TO so_mover")?;
        assert_eq!(
            as_role("so_mover", "warren.set_owner('so', 'so_new')")?.as_deref(),
            Some(
                "42501: must be a member of role \"so_owner\" to change the owner of graph \"so\""
            )
        );
        Ok(())
    }

    #[pg_test]
    fn the_caller_must_be_able_to_become_the_new_owner() -> Result<(), Error> {
        graph()?;
        Spi::run("CREATE ROLE so_mover; GRANT so_owner TO so_mover")?;
        assert_eq!(
            as_role("so_mover", "warren.set_owner('so', 'so_new')")?.as_deref(),
            Some("42501: must be able to SET ROLE \"so_new\" to make it the owner of graph \"so\"")
        );
        Spi::run("GRANT so_new TO so_mover")?;
        assert_eq!(as_role("so_mover", "warren.set_owner('so', 'so_new')")?, None);
        assert_eq!(owner()?.as_deref(), Some("so_new"));
        Ok(())
    }

    /// PG 16+: `ALTER … OWNER TO` needs the `SET` option of the membership
    /// (`check_can_set_role`); PG 15 has no such option.
    #[cfg(not(feature = "pg15"))]
    #[pg_test]
    fn a_membership_without_the_set_option_is_not_enough() -> Result<(), Error> {
        graph()?;
        Spi::run("CREATE ROLE so_mover; GRANT so_owner TO so_mover; GRANT so_new TO so_mover WITH SET FALSE")?;
        assert_eq!(
            as_role("so_mover", "warren.set_owner('so', 'so_new')")?.as_deref(),
            Some("42501: must be able to SET ROLE \"so_new\" to make it the owner of graph \"so\"")
        );
        Ok(())
    }

    /// A contraction has no source queries to open.
    #[pg_test]
    fn a_contraction_changes_owner_without_sources() -> Result<(), Error> {
        graph()?;
        Spi::run(
            "SELECT warren.vector_const('so', 'c', 1);
             SELECT warren.contract('so', 'c', 'so_q');
             REVOKE SELECT ON so_nodes, so_edges FROM so_new;
             SELECT warren.set_owner('so_q', 'so_new')",
        )?;
        let owner =
            Spi::get_one::<String>("SELECT owner::text FROM warren.graphs WHERE name = 'so_q'")?;
        assert_eq!(owner.as_deref(), Some("so_new"));
        Ok(())
    }

    #[pg_test]
    fn a_role_that_does_not_exist_cannot_own_a_graph() -> Result<(), Error> {
        graph()?;
        let gone =
            Spi::get_one::<pg_sys::Oid>("CREATE ROLE so_gone; SELECT 'so_gone'::regrole::oid")?
                .unwrap_or(pg_sys::Oid::INVALID);
        Spi::run("DROP ROLE so_gone")?;
        assert_eq!(
            failure(&format!("warren.set_owner('so', {}::oid::regrole)", gone.to_u32()))?
                .as_deref(),
            Some(format!("42704: role with OID {} does not exist", gone.to_u32()).as_str())
        );
        Ok(())
    }

    /// DESIGN.md §7: the `search_path` notice is judged for the new owner,
    /// who may create in a schema the old owner may not.
    #[pg_test]
    fn writable_schemas_are_judged_for_the_new_owner() -> Result<(), Error> {
        graph()?;
        // The judgement alone: `set_owner`'s NOTICE itself goes to the
        // client, which a pg_test cannot read (test/sql/search_path_notice.sql
        // reads it from the server log).
        Spi::run("CREATE SCHEMA so_mine; GRANT CREATE ON SCHEMA so_mine TO so_new")?;
        let new = Spi::get_one::<pg_sys::Oid>("SELECT 'so_new'::regrole::oid")?
            .unwrap_or(pg_sys::Oid::INVALID);
        let old = Spi::get_one::<pg_sys::Oid>("SELECT 'so_owner'::regrole::oid")?
            .unwrap_or(pg_sys::Oid::INVALID);
        let found = |owner| {
            super::super::create_graph::writable_schemas("so_mine, public", owner)
                .map_err(|_| Error::InvalidPosition)
        };
        assert_eq!(found(old)?.as_deref(), Some("so_mine"));
        assert_eq!(found(new)?, None);
        Ok(())
    }

    /// `so`, its owner dropped without `DROP OWNED` (DESIGN.md §5.7).
    fn orphan() -> Result<(), Error> {
        graph()?;
        Spi::run("REVOKE ALL ON so_nodes, so_edges FROM so_owner; DROP ROLE so_owner")
    }

    /// DESIGN.md §14 item 45: a superuser may adopt an orphan.
    #[pg_test]
    fn a_superuser_may_adopt_a_graph_whose_owner_was_dropped() -> Result<(), Error> {
        orphan()?;
        Spi::run("SELECT warren.set_owner('so', 'so_new')")?;
        assert_eq!(owner()?.as_deref(), Some("so_new"));
        assert_eq!(as_role("so_new", "warren.refresh('so')")?, None);
        Ok(())
    }

    /// DESIGN.md §14 item 45: the extension's owner may drop an orphan but
    /// not adopt it, which would hand it what the dropped owner could see.
    #[pg_test]
    fn only_a_superuser_may_adopt_a_graph_whose_owner_was_dropped() -> Result<(), Error> {
        orphan()?;
        Spi::run(
            "CREATE ROLE so_installer; GRANT so_new TO so_installer;
             UPDATE pg_catalog.pg_extension SET extowner = 'so_installer'::regrole
             WHERE extname = 'pg_warren'",
        )?;
        assert_eq!(
            as_role("so_installer", "warren.set_owner('so', 'so_new')")?.as_deref(),
            Some(
                "42501: only a superuser may give graph \"so\", whose owner no longer exists, \
                 a new owner"
            )
        );
        Ok(())
    }
}
