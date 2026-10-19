//! `warren.drop_version` and `warren.drop_graph` (DESIGN.md §7): explicit
//! deletion, which removes segments and bound vectors with the version.

use pgrx::prelude::*;

use super::version::drop_version as delete_version;
use super::{CatalogError, GraphDefinition, VersionId};
use crate::access::{entry, is_superuser, require_owner, AccessError};

/// `warren.drop_version`: deletes a version of graph `name` that is not
/// its current version, even one with bound vectors. The caller must be a
/// member of the owner role.
#[pg_extern(schema = "warren")]
fn drop_version(name: &str, version: i64) {
    entry(|_| drop_version_by_name(name, version));
}

fn drop_version_by_name(name: &str, version: VersionId) -> Result<(), CatalogError> {
    let mut graph = GraphDefinition::find(name)?;
    require_owner(graph.owner, &graph.name, "drop versions of")?;
    graph.lock()?;
    delete_version(&graph, version)
}

/// `warren.drop_graph`: deletes graph `name` and everything built from it.
/// The caller must be a member of the owner role; if the owner role was
/// dropped, a superuser may still drop the graph.
#[pg_extern(schema = "warren")]
fn drop_graph(name: &str) {
    entry(|_| drop_graph_by_name(name));
}

fn drop_graph_by_name(name: &str) -> Result<(), CatalogError> {
    let graph = GraphDefinition::find(name)?;
    match require_owner(graph.owner, &graph.name, "drop") {
        Err(AccessError::OwnerDropped(_)) if is_superuser()? => {}
        checked => checked?,
    }
    graph.drop_graph()
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, refresh, ring, versions};

    fn rows(version: i64) -> Result<Option<i64>, Error> {
        Spi::get_one::<i64>(&format!(
            "SELECT (SELECT count(*) FROM warren.segments WHERE version_id = {version})
                  + (SELECT count(*) FROM warren.vectors WHERE version_id = {version})"
        ))
    }

    #[pg_test]
    fn drop_version_removes_a_version_its_segments_and_its_vectors() -> Result<(), Error> {
        ring("d", 5, 2)?;
        let first = refresh("d", true)?;
        let staged = refresh("d", false)?;
        Spi::run(&format!(
            "INSERT INTO warren.vectors (graph_id, version_id, name, dtype, length)
             SELECT graph_id, {staged}, 'rank', 'f64', 5 FROM warren.graphs WHERE name = 'd'"
        ))?;
        Spi::run(&format!("SELECT warren.drop_version('d', {staged})"))?;
        assert_eq!(versions("d")?, Some(format!("{first}:false")));
        assert_eq!(rows(staged)?, Some(0));
        Ok(())
    }

    #[pg_test]
    fn the_current_version_cannot_be_dropped() -> Result<(), Error> {
        ring("d", 5, 2)?;
        let first = refresh("d", true)?;
        assert_eq!(
            failure(&format!("warren.drop_version('d', {first})"))?,
            Some(format!(
                "55006: cannot drop version {first} of graph \"d\": it is the current version; \
                 promote another first"
            ))
        );
        Ok(())
    }

    #[pg_test(error = "graph \"d\" has no version 999")]
    fn dropping_an_unknown_version_fails() -> Result<(), Error> {
        ring("d", 5, 2)?;
        Spi::run("SELECT warren.drop_version('d', 999)")
    }

    #[pg_test]
    fn drop_graph_removes_the_graph_and_everything_built_from_it() -> Result<(), Error> {
        ring("d", 5, 2)?;
        let first = refresh("d", true)?;
        refresh("d", false)?;
        Spi::run("SELECT warren.drop_graph('d')")?;
        let left = Spi::get_one::<i64>(
            "SELECT (SELECT count(*) FROM warren.graphs) + (SELECT count(*) FROM warren.versions)
                  + (SELECT count(*) FROM warren.segments)",
        )?;
        assert_eq!(left, Some(0));
        assert_eq!(rows(first)?, Some(0));
        Ok(())
    }

    #[pg_test(error = "must be a member of role \"d_owner\" to drop graph \"d\"")]
    fn only_the_owner_may_drop_a_graph() -> Result<(), Error> {
        ring("d", 5, 2)?;
        Spi::run("CREATE ROLE dropper; SET ROLE dropper; SELECT warren.drop_graph('d')")
    }

    #[pg_test]
    fn a_superuser_may_drop_a_graph_whose_owner_was_dropped() -> Result<(), Error> {
        Spi::run(
            "CREATE ROLE doomed;
             CREATE TABLE doomed_nodes (id bigint);
             CREATE TABLE doomed_edges (src bigint, dst bigint);
             GRANT SELECT ON doomed_nodes, doomed_edges TO doomed;
             SET ROLE doomed;
             SELECT warren.create_graph('doomed', 'SELECT id FROM doomed_nodes',
                                        'SELECT src, dst FROM doomed_edges');
             RESET ROLE;
             REVOKE ALL ON doomed_nodes, doomed_edges FROM doomed;
             DROP ROLE doomed;
             SELECT warren.drop_graph('doomed')",
        )?;
        assert_eq!(Spi::get_one::<i64>("SELECT count(*) FROM warren.graphs")?, Some(0));
        Ok(())
    }
}
