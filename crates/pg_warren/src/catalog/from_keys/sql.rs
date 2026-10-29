//! `warren.queries_from_keys` and `warren.create_graph_from_keys`
//! (DESIGN.md §9.3, §14 item 52).

use pgrx::prelude::*;

use super::queries::{queries, Queries};
use super::read::describe;
use crate::access::{current_user, entry, TRUSTED_SEARCH_PATH};
use crate::catalog::create_graph::record;
use crate::catalog::{parse_node_order, CatalogError, GraphSource, NewGraph};

/// `warren.queries_from_keys(tables)`: the node and edge queries of the
/// graph over `tables`' rows and foreign keys, and whether the edge query
/// has a relation column, for `warren.create_graph`. It reads only the
/// catalog, which every role may: `PARALLEL SAFE`.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren."queries_from_keys"(tables regclass[])
RETURNS TABLE (node_query text, edge_query text, has_relations boolean)
STRICT STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'queries_from_keys_wrapper';
"#
)]
fn queries_from_keys(
    tables: Vec<Option<pg_sys::Oid>>,
) -> TableIterator<
    'static,
    (name!(node_query, String), name!(edge_query, String), name!(has_relations, bool)),
> {
    entry(|_| {
        let found = generated(&tables)?;
        Ok::<_, CatalogError>(TableIterator::new(std::iter::once((
            found.node_query,
            found.edge_query,
            found.has_relations,
        ))))
    })
}

/// `warren.create_graph_from_keys(name, tables, ...)`: records graph
/// `name` over the queries `queries_from_keys(tables)` gives, as
/// `warren.create_graph` does (its other options but `weighted` and
/// `has_relations`, which the keys decide): owned by the caller, and both
/// queries opened as the caller, so the caller must be able to read every
/// listed table. The queries name every object they use with its schema,
/// so they are recorded with the trusted `search_path`, `pg_catalog,
/// pg_temp`, rather than the caller's: no schema another role can write is
/// ever searched when they run.
#[allow(clippy::too_many_arguments)] // create_graph's options
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren."create_graph_from_keys"(name text, tables regclass[],
                                                directed boolean DEFAULT true,
                                                build_reverse boolean DEFAULT true,
                                                retain_versions int DEFAULT 2,
                                                store_merged boolean DEFAULT true,
                                                node_order text DEFAULT 'id')
RETURNS void STRICT VOLATILE PARALLEL UNSAFE LANGUAGE c
AS 'MODULE_PATHNAME', 'create_graph_from_keys_wrapper';
"#
)]
fn create_graph_from_keys(
    name: &str,
    tables: Vec<Option<pg_sys::Oid>>,
    directed: bool,
    build_reverse: bool,
    retain_versions: i32,
    store_merged: bool,
    node_order: &str,
) {
    entry(|_| {
        let node_order = parse_node_order(node_order)?;
        let found = generated(&tables)?;
        record(&NewGraph {
            node_order,
            name,
            source: GraphSource::Query,
            owner: current_user(),
            search_path: TRUSTED_SEARCH_PATH,
            node_query: Some(&found.node_query),
            edge_query: Some(&found.edge_query),
            id_type: pg_sys::Oid::INVALID,
            directed,
            weighted: false,
            has_relations: found.has_relations,
            build_reverse,
            store_merged,
            retain_versions,
        })
    });
}

fn generated(tables: &[Option<pg_sys::Oid>]) -> Result<Queries, CatalogError> {
    Ok(queries(&describe(tables)?)?)
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, failure_as};

    /// Customers, their orders, products, and order_lines, a junction of
    /// orders and products; in schema `shop`, off the search_path.
    fn shop() -> Result<(), Error> {
        Spi::run(
            "CREATE SCHEMA shop;
             CREATE TABLE shop.customers (id bigint PRIMARY KEY, name text);
             CREATE TABLE shop.orders (id bigint PRIMARY KEY, customer_id bigint REFERENCES shop.customers);
             CREATE TABLE shop.products (sku text PRIMARY KEY);
             CREATE TABLE shop.order_lines (order_id bigint REFERENCES shop.orders,
                                            sku text REFERENCES shop.products,
                                            PRIMARY KEY (order_id, sku));
             INSERT INTO shop.customers VALUES (1, 'ada'), (2, 'bob');
             INSERT INTO shop.orders VALUES (10, 1), (11, 1), (12, NULL);
             INSERT INTO shop.products VALUES ('pen'), ('ink');
             INSERT INTO shop.order_lines VALUES (10, 'pen'), (10, 'ink'), (12, 'ink')",
        )
    }

    const SHOP: &str =
        "ARRAY['shop.customers', 'shop.orders', 'shop.products', 'shop.order_lines']::regclass[]";

    #[pg_test]
    fn the_queries_return_the_tables_rows_and_links() -> Result<(), Error> {
        shop()?;
        Spi::run(&format!(
            "CREATE TEMP TABLE q AS SELECT * FROM warren.queries_from_keys({SHOP})"
        ))?;
        let relations = Spi::get_one::<bool>("SELECT has_relations FROM q")?;
        assert_eq!(relations, Some(true));
        let node_query = Spi::get_one::<String>("SELECT node_query FROM q")?.unwrap_or_default();
        let nodes = Spi::get_one::<String>(&format!(
            "SELECT string_agg(id, ' ' ORDER BY id) FROM ({node_query}) n (id)"
        ))?;
        assert_eq!(
            nodes.as_deref(),
            Some(
                "shop.customers:1 shop.customers:2 shop.orders:10 shop.orders:11 shop.orders:12 \
                 shop.products:ink shop.products:pen"
            ),
            "order_lines is no table of nodes"
        );
        let edge_query = Spi::get_one::<String>("SELECT edge_query FROM q")?.unwrap_or_default();
        let edges = Spi::get_one::<String>(&format!(
            "SELECT string_agg(src || '>' || dst || ' ' || rel, ', ' ORDER BY src, dst)
             FROM ({edge_query}) e (src, dst, rel)"
        ))?;
        assert_eq!(
            edges.as_deref(),
            Some(
                "shop.orders:10>shop.customers:1 orders_customer_id_fkey, \
                 shop.orders:10>shop.products:ink order_lines, \
                 shop.orders:10>shop.products:pen order_lines, \
                 shop.orders:11>shop.customers:1 orders_customer_id_fkey, \
                 shop.orders:12>shop.products:ink order_lines"
            ),
            "order 12's NULL customer is no edge"
        );
        Ok(())
    }

    #[pg_test]
    fn a_graph_from_keys_is_built_and_traversed() -> Result<(), Error> {
        shop()?;
        Spi::run(&format!(
            "SELECT warren.create_graph_from_keys('shop', {SHOP}, directed := false,
                                                  retain_versions := 3);
             SELECT warren.refresh('shop')"
        ))?;
        let recorded = Spi::get_one::<String>(
            "SELECT format('%s|%s|%s|%s|%s|%s', id_type, has_relations, weighted, directed,
                           retain_versions, search_path)
             FROM warren.graphs WHERE name = 'shop'",
        )?;
        assert_eq!(recorded.as_deref(), Some("text|t|f|f|3|pg_catalog, pg_temp"));
        let reached = Spi::get_one::<String>(
            "SELECT string_agg(node_id || '@' || depth, ' ' ORDER BY node_id)
             FROM warren.bfs('shop', ARRAY['shop.customers:2', 'shop.products:pen'], 2)",
        )?;
        assert_eq!(
            reached.as_deref(),
            Some(
                "shop.customers:1@2 shop.customers:2@0 shop.orders:10@1 shop.products:ink@2 \
                 shop.products:pen@0"
            )
        );
        Ok(())
    }

    #[pg_test]
    fn one_table_keeps_its_own_ids() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE staff (id int PRIMARY KEY, manager int REFERENCES staff);
             INSERT INTO staff VALUES (1, NULL), (2, 1), (3, 2);
             SELECT warren.create_graph_from_keys('staff', ARRAY['staff']::regclass[]);
             SELECT warren.refresh('staff')",
        )?;
        let path = Spi::get_one::<Vec<i32>>("SELECT warren.shortest_path('staff', 3, 1)")?;
        assert_eq!(path, Some(vec![3, 2, 1]));
        let flags = Spi::get_one::<String>(
            "SELECT id_type || ' ' || has_relations FROM warren.graphs WHERE name = 'staff'",
        )?;
        assert_eq!(flags.as_deref(), Some("integer false"));
        Ok(())
    }

    /// The caller must be able to read every listed table, as
    /// `create_graph` checks by opening the queries (DESIGN.md §7).
    #[pg_test]
    fn a_role_that_cannot_read_a_listed_table_cannot_create_the_graph() -> Result<(), Error> {
        shop()?;
        Spi::run(
            "CREATE ROLE fk_clerk;
             GRANT USAGE ON SCHEMA shop TO fk_clerk;
             GRANT SELECT ON shop.customers, shop.orders, shop.order_lines TO fk_clerk",
        )?;
        let refused =
            failure_as("fk_clerk", &format!("warren.create_graph_from_keys('s', {SHOP})"))?;
        assert_eq!(refused.as_deref(), Some("42501: permission denied for table products"));
        let by_hand = failure_as(
            "fk_clerk",
            "warren.create_graph('s', 'SELECT sku FROM shop.products', 'SELECT sku, sku FROM shop.products')",
        )?;
        assert_eq!(by_hand, refused, "as create_graph refuses it");
        let generated = failure_as("fk_clerk", &format!("warren.queries_from_keys({SHOP})"))?;
        assert_eq!(generated, None, "the catalog is readable");
        Spi::run("GRANT SELECT ON shop.products TO fk_clerk")?;
        assert_eq!(
            failure_as("fk_clerk", &format!("warren.create_graph_from_keys('s', {SHOP})"))?,
            None
        );
        let owner =
            Spi::get_one::<String>("SELECT owner::text FROM warren.graphs WHERE name = 's'")?;
        assert_eq!(owner.as_deref(), Some("fk_clerk"));
        Ok(())
    }

    #[pg_test]
    fn refusals_have_their_sqlstates() -> Result<(), Error> {
        Spi::run("CREATE TABLE fk_log (at timestamptz)")?;
        let refused = |tables: &str| failure(&format!("warren.queries_from_keys({tables})"));
        assert_eq!(
            refused("'{}'::regclass[]")?.as_deref(),
            Some("22023: tables must list at least one table")
        );
        assert_eq!(
            refused("ARRAY[NULL]::regclass[]")?.as_deref(),
            Some("22004: tables must not hold NULL")
        );
        assert_eq!(
            refused("ARRAY['fk_log']::regclass[]")?.as_deref(),
            Some(
                "55000: table public.fk_log has no primary key: its rows need one to be nodes; \
                 add one, or write the graph's node and edge queries with warren.create_graph"
            )
        );
        let graphs = Spi::get_one::<i64>("SELECT count(*) FROM warren.graphs")?;
        assert_eq!(graphs, Some(0));
        Ok(())
    }
}
