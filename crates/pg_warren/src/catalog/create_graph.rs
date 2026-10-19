//! `warren.create_graph`: records a graph definition after checking its
//! source queries (DESIGN.md §9.3, §5.1).

use pgrx::prelude::*;

use super::{CatalogError, NewGraph};
use crate::access::{current_user, entry, with_search_path};
use crate::source::{check_edge_columns, check_node_columns, source_columns};

/// `warren.create_graph`: records graph `name` over `node_query` and
/// `edge_query`, owned by the calling role, with the caller's
/// `search_path`, under which every refresh will run the queries. Both queries are opened (not
/// run) as the caller to check their columns; the node query's column type
/// becomes the graph's `id_type`.
#[allow(clippy::too_many_arguments)] // the SQL signature of DESIGN.md §9.3
#[pg_extern(schema = "warren")]
fn create_graph(
    name: &str,
    node_query: &str,
    edge_query: &str,
    directed: default!(bool, true),
    weighted: default!(bool, false),
    has_relations: default!(bool, false),
    build_reverse: default!(bool, true),
    retain_versions: default!(i32, 2),
) {
    entry(|caller| {
        record(&NewGraph {
            name,
            owner: current_user(),
            search_path: caller.search_path(),
            node_query,
            edge_query,
            id_type: pg_sys::Oid::INVALID,
            directed,
            weighted,
            has_relations,
            build_reverse,
            retain_versions,
        })
    });
}

fn record(graph: &NewGraph<'_>) -> Result<(), CatalogError> {
    if graph.retain_versions < 1 {
        return Err(CatalogError::RetainVersions(graph.retain_versions));
    }
    // The queries are the caller's own SQL: check them under its search_path.
    let (node_columns, edge_columns) = with_search_path(graph.search_path, || {
        Ok::<_, CatalogError>((
            source_columns::<CatalogError>("node_query", graph.node_query)?,
            source_columns::<CatalogError>("edge_query", graph.edge_query)?,
        ))
    })?;
    let id_type = check_node_columns(&node_columns)?;
    check_edge_columns(&edge_columns, id_type, graph.has_relations, graph.weighted)?;
    NewGraph { id_type: id_type.oid(), ..*graph }.create()
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    fn tables() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE cg_nodes (id bigint, small int, u uuid);
             CREATE TABLE cg_edges (src bigint, dst bigint, rel text, w float8)",
        )
    }

    #[pg_test]
    fn create_graph_records_the_definition() -> Result<(), Error> {
        tables()?;
        Spi::run(
            "SELECT warren.create_graph('cg', 'SELECT id FROM cg_nodes',
                 'SELECT src, dst, rel, w FROM cg_edges', directed := false,
                 weighted := true, has_relations := true, build_reverse := false,
                 retain_versions := 3)",
        )?;
        let row = Spi::get_one::<String>(
            "SELECT format('%s|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s', owner = current_user::regrole,
                           readers, source, node_query, edge_query, id_type, directed, weighted,
                           has_relations, build_reverse, retain_versions,
                           current_version IS NULL)
             FROM warren.graphs WHERE name = 'cg'",
        )?;
        assert_eq!(
            row.as_deref(),
            Some(
                "t|{}|query|SELECT id FROM cg_nodes|SELECT src, dst, rel, w FROM cg_edges|\
                 bigint|f|t|t|f|3|t"
            )
        );
        Ok(())
    }

    #[pg_test]
    fn create_graph_defaults_match_the_design() -> Result<(), Error> {
        tables()?;
        Spi::run(
            "SELECT warren.create_graph('cg', 'SELECT id FROM cg_nodes',
                                        'SELECT src, dst FROM cg_edges')",
        )?;
        let flags = Spi::get_one::<String>(
            "SELECT format('%s|%s|%s|%s|%s', directed, weighted, has_relations, build_reverse,
                           retain_versions)
             FROM warren.graphs WHERE name = 'cg'",
        )?;
        assert_eq!(flags.as_deref(), Some("t|f|f|t|2"));
        Ok(())
    }

    #[pg_test]
    fn the_creating_role_owns_the_graph() -> Result<(), Error> {
        tables()?;
        Spi::run(
            "CREATE ROLE cg_creator;
             GRANT SELECT ON cg_nodes, cg_edges TO cg_creator;
             SET ROLE cg_creator;
             SELECT warren.create_graph('cg', 'SELECT id FROM cg_nodes',
                                        'SELECT src, dst FROM cg_edges');
             RESET ROLE",
        )?;
        let owner = Spi::get_one::<String>("SELECT owner::text FROM warren.graphs")?;
        assert_eq!(owner.as_deref(), Some("cg_creator"));
        Ok(())
    }

    #[pg_test(error = "node_query must return 1 columns, not 2")]
    fn a_node_query_with_two_columns_is_rejected() -> Result<(), Error> {
        tables()?;
        Spi::run(
            "SELECT warren.create_graph('cg', 'SELECT id, id FROM cg_nodes',
                                        'SELECT src, dst FROM cg_edges')",
        )
    }

    #[pg_test]
    fn int4_and_uuid_node_ids_are_id_types() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE cg_small (src int, dst int); CREATE TABLE cg_u (src uuid, dst uuid)",
        )?;
        tables()?;
        Spi::run(
            "SELECT warren.create_graph('cg4', 'SELECT small FROM cg_nodes',
                                        'SELECT src, dst FROM cg_small');
             SELECT warren.create_graph('cgu', 'SELECT u FROM cg_nodes',
                                        'SELECT src, dst FROM cg_u')",
        )?;
        let types = Spi::get_one::<String>(
            "SELECT string_agg(id_type::text, ' ' ORDER BY name) FROM warren.graphs",
        )?;
        assert_eq!(types.as_deref(), Some("integer uuid"));
        Ok(())
    }

    #[pg_test(error = "edge_query column 1 (source id) must be of type uuid, not bigint")]
    fn edge_endpoints_must_have_the_node_id_type() -> Result<(), Error> {
        tables()?;
        Spi::run(
            "SELECT warren.create_graph('cg', 'SELECT u FROM cg_nodes',
                                        'SELECT src, dst FROM cg_edges')",
        )
    }

    #[pg_test(error = "edge_query column 3 (weight) must be of type double precision, not text")]
    fn an_edge_query_must_match_the_flags() -> Result<(), Error> {
        tables()?;
        Spi::run(
            "SELECT warren.create_graph('cg', 'SELECT id FROM cg_nodes',
                                        'SELECT src, dst, rel FROM cg_edges', weighted := true)",
        )
    }

    // Source queries run as read-only cursors, which reject writes.
    #[pg_test(error = "INSERT is not allowed in a non-volatile function")]
    fn a_source_query_must_be_read_only() -> Result<(), Error> {
        tables()?;
        Spi::run(
            "SELECT warren.create_graph('cg', 'INSERT INTO cg_nodes VALUES (1) RETURNING id',
                                        'SELECT src, dst FROM cg_edges')",
        )
    }

    // ... even through a VOLATILE function the query calls.
    #[pg_test(error = "cannot execute INSERT in a read-only transaction")]
    fn a_source_query_cannot_write_through_a_function() -> Result<(), Error> {
        tables()?;
        Spi::run(
            "INSERT INTO cg_nodes (id) VALUES (1);
             CREATE FUNCTION cg_write() RETURNS boolean VOLATILE LANGUAGE plpgsql AS
                 $f$ BEGIN INSERT INTO cg_nodes VALUES (99); RETURN true; END $f$;
             SELECT warren.create_graph('cg', 'SELECT id FROM cg_nodes WHERE cg_write()',
                                        'SELECT src, dst FROM cg_edges');
             SELECT warren.refresh('cg')",
        )
    }

    #[pg_test(error = "graph \"cg\" already exists")]
    fn graph_names_are_unique() -> Result<(), Error> {
        tables()?;
        Spi::run(
            "SELECT warren.create_graph('cg', 'SELECT id FROM cg_nodes',
                                        'SELECT src, dst FROM cg_edges');
             SELECT warren.create_graph('cg', 'SELECT id FROM cg_nodes',
                                        'SELECT src, dst FROM cg_edges')",
        )
    }

    #[pg_test(error = "retain_versions must be at least 1, not 0")]
    fn at_least_one_version_is_retained() -> Result<(), Error> {
        tables()?;
        Spi::run(
            "SELECT warren.create_graph('cg', 'SELECT id FROM cg_nodes',
                                        'SELECT src, dst FROM cg_edges', retain_versions := 0)",
        )
    }
}
