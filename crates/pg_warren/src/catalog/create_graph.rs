//! `warren.create_graph`: records a graph definition after checking its
//! source queries (DESIGN.md §9.3, §5.1).

use pgrx::prelude::*;

use super::{parse_node_order, CatalogError, NewGraph};
use crate::access::{current_user, entry, with_search_path};
use crate::read_only::read_one;
use crate::source::{check_edge_columns, check_node_columns, source_columns};

/// Of the schemas in `$1`, those, in order and quoted, in which PUBLIC or
/// a role other than the caller that is not a superuser and not predefined
/// (`pg_*`; `pg_database_owner` counts through the database's owner) may
/// create objects. (Membership is not excluded: a superuser is a member of
/// every role, and a group role's other members could create there too.)
/// Temporary schemas are left out: Postgres reports CREATE on this
/// session's own for every role with TEMP, but no other session can use
/// it.
const WRITABLE_SCHEMAS: &str = "SELECT pg_catalog.string_agg(pg_catalog.quote_ident(s.name), ', '
                                                ORDER BY s.position)
     FROM pg_catalog.unnest($1::pg_catalog.name[]) WITH ORDINALITY AS s (name, position)
     JOIN pg_catalog.pg_namespace n ON n.nspname = s.name
     WHERE n.oid <> pg_catalog.pg_my_temp_schema() AND NOT pg_catalog.pg_is_other_temp_schema(n.oid)
       AND (pg_catalog.has_schema_privilege('public', n.oid, 'CREATE')
            OR EXISTS (SELECT FROM pg_catalog.pg_roles r
                       WHERE NOT r.rolsuper AND NOT pg_catalog.starts_with(r.rolname, 'pg_')
                         AND r.rolname <> pg_catalog.current_user()
                         AND pg_catalog.has_schema_privilege(r.oid, n.oid, 'CREATE')))";

/// `warren.create_graph`: records graph `name` over `node_query` and
/// `edge_query`, owned by the calling role, with the caller's
/// `search_path`, under which every refresh will run the queries. Both queries are opened (not
/// run) as the caller to check their columns; the node query's column type
/// becomes the graph's `id_type`. With several relations, `store_merged`
/// stores the merged CSR; without it, all-relations reads merge the
/// relations' rows on the fly, with the same results, in less memory and
/// more time (DESIGN.md §5.3). `node_order` orders the dense ids of the
/// first build: `'id'` in external-id order, `'locality'` so that
/// neighbours get nearby ids (DESIGN.md §14 item 22).
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
    store_merged: default!(bool, true),
    node_order: default!(&str, "'id'"),
) {
    entry(|caller| {
        record(&NewGraph {
            node_order: parse_node_order(node_order)?,
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
            store_merged,
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
    NewGraph { id_type: id_type.oid(), ..*graph }.create()?;
    if let Some(schemas) = writable_schemas(graph.search_path)? {
        notice!("{}", writable_notice(graph.name, graph.search_path, &schemas));
    }
    Ok(())
}

/// The schemas of `search_path` that roles other than the caller can create
/// objects in (`WRITABLE_SCHEMAS`), comma-separated, or `None`. A function
/// or operator planted there can capture an unqualified name in the source
/// queries, which every refresh runs as the graph owner (DESIGN.md §7).
fn writable_schemas(search_path: &str) -> Result<Option<String>, CatalogError> {
    // Only the effective schemas are read under that search_path; the
    // check runs under the pinned one.
    let schemas = with_search_path(search_path, || {
        read_one::<Vec<String>>("SELECT pg_catalog.current_schemas(false)::pg_catalog.text[]", &[])
    })?
    .unwrap_or_default();
    Ok(read_one::<String>(WRITABLE_SCHEMAS, &[schemas.into()])?)
}

fn writable_notice(graph: &str, search_path: &str, schemas: &str) -> String {
    format!(
        "graph \"{graph}\" runs its source queries under search_path \"{search_path}\", in which \
         other roles can create objects (in {schemas}): a function or operator created there \
         could run as the graph's owner at every refresh; schema-qualify the source queries' \
         functions and operators, or create the graph under a search_path of schemas only its \
         owner can write"
    )
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    fn tables() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE cg_nodes (id bigint, small int, u uuid);
             CREATE TABLE cg_edges (src bigint, dst bigint, rel text, w float8)",
        )
    }

    /// DESIGN.md §7: a schema on the graph's search_path that another
    /// role can create in lets that role plant functions or operators that
    /// refresh runs as the graph owner.
    #[pg_test]
    fn schemas_others_can_create_in_are_found_on_the_search_path() -> Result<(), Error> {
        Spi::run(
            "CREATE ROLE cg_other;
             CREATE SCHEMA cg_shared; GRANT CREATE ON SCHEMA cg_shared TO cg_other;
             CREATE SCHEMA cg_private; GRANT USAGE ON SCHEMA cg_private TO cg_other",
        )?;
        let found = |path: &str| super::writable_schemas(path).map_err(|_| Error::InvalidPosition);
        assert_eq!(found("cg_private, cg_shared, public")?.as_deref(), Some("cg_shared"));
        assert_eq!(found("cg_private")?, None, "only the owner (a superuser) can create there");
        assert_eq!(found("public")?, None, "PG 15+: only the database owner, here a superuser");
        assert_eq!(found("cg_missing, pg_catalog")?, None, "missing and system schemas");
        Ok(())
    }

    #[pg_test]
    fn a_schema_public_can_create_in_is_found_without_other_roles() -> Result<(), Error> {
        Spi::run("CREATE SCHEMA cg_open; GRANT CREATE ON SCHEMA cg_open TO PUBLIC")?;
        let found = super::writable_schemas("cg_open").map_err(|_| Error::InvalidPosition)?;
        assert_eq!(found.as_deref(), Some("cg_open"));
        Ok(())
    }

    /// Every role with TEMP on the database has CREATE on this session's
    /// own temporary schema, as Postgres reports it; nobody else can use it.
    #[pg_test]
    fn the_sessions_temporary_schema_is_not_found() -> Result<(), Error> {
        Spi::run("CREATE ROLE cg_temp_other; CREATE TEMP TABLE cg_temp (x int)")?;
        let found = super::writable_schemas("pg_temp").map_err(|_| Error::InvalidPosition)?;
        assert_eq!(found, None);
        Ok(())
    }

    #[pg_test]
    fn schema_names_are_quoted() -> Result<(), Error> {
        Spi::run(
            r#"CREATE ROLE cg_quoted; CREATE SCHEMA "cg Odd"; GRANT CREATE ON SCHEMA "cg Odd" TO cg_quoted"#,
        )?;
        let found = super::writable_schemas(r#""cg Odd""#).map_err(|_| Error::InvalidPosition)?;
        assert_eq!(found.as_deref(), Some(r#""cg Odd""#));
        Ok(())
    }

    #[pg_test]
    fn create_graph_records_the_definition() -> Result<(), Error> {
        tables()?;
        Spi::run(
            "SELECT warren.create_graph('cg', 'SELECT id FROM cg_nodes',
                 'SELECT src, dst, rel, w FROM cg_edges', directed := false,
                 weighted := true, has_relations := true, build_reverse := false,
                 retain_versions := 3, store_merged := false, node_order := 'locality')",
        )?;
        let row = Spi::get_one::<String>(
            "SELECT format('%s|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s',
                           owner = current_user::regrole, readers, source, node_query,
                           edge_query, id_type, directed, weighted, has_relations,
                           build_reverse, retain_versions, current_version IS NULL,
                           store_merged, node_order)
             FROM warren.graphs WHERE name = 'cg'",
        )?;
        assert_eq!(
            row.as_deref(),
            Some(
                "t|{}|query|SELECT id FROM cg_nodes|SELECT src, dst, rel, w FROM cg_edges|\
                 bigint|f|t|t|f|3|t|f|locality"
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
            "SELECT format('%s|%s|%s|%s|%s|%s|%s', directed, weighted, has_relations,
                           build_reverse, retain_versions, store_merged, node_order)
             FROM warren.graphs WHERE name = 'cg'",
        )?;
        assert_eq!(flags.as_deref(), Some("t|f|f|t|2|t|id"));
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

    /// DESIGN.md §14 item 22: the node order is 'id' or 'locality'.
    #[pg_test]
    fn an_unknown_node_order_is_an_invalid_parameter() -> Result<(), Error> {
        tables()?;
        let refused = crate::fixture::failure(
            "warren.create_graph('cg', 'SELECT id FROM cg_nodes', 'SELECT src, dst FROM cg_edges',
                                 node_order := 'random')",
        )?;
        assert_eq!(
            refused.as_deref(),
            Some("22023: node_order must be 'id' or 'locality', not 'random'")
        );
        let graphs = Spi::get_one::<i64>("SELECT count(*) FROM warren.graphs")?;
        assert_eq!(graphs, Some(0));
        Ok(())
    }
}
