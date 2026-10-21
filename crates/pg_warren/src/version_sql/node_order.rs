//! Tests: a graph ordered for locality answers every query as the same
//! graph in id order does, read by external id, before and across a
//! compaction, and later refreshes keep its ids (DESIGN.md §14 item 22).

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{current, refresh, rows};

    /// Graphs `<name>_i` (node order 'id') and `<name>_l` ('locality') over
    /// the same tables of `int8` ids, owned by role `<name>_owner`: about
    /// 900 weighted edges in three relations over 120 nodes, with
    /// self-loops, isolated nodes and parallel edges, built once.
    fn order_pair(name: &str) -> Result<(), Error> {
        Spi::run(&format!(
            "CREATE ROLE {name}_owner;
             CREATE TABLE {name}_nodes (id bigint);
             CREATE TABLE {name}_edges (src bigint, dst bigint, rel text, w float8);
             INSERT INTO {name}_nodes SELECT generate_series(1, 120);
             INSERT INTO {name}_edges
                 SELECT s, d, 'r' || (s * 7 + d) % 3, ((s * d) % 10) / 10.0 + 0.1
                 FROM generate_series(1, 110) s, generate_series(1, 110) d
                 WHERE (s * 31 + d * 17) % 23 = 0 OR (s = d AND s % 9 = 0);
             INSERT INTO {name}_edges SELECT src, dst, rel, 0.3 FROM {name}_edges
                 WHERE (src + dst) % 5 = 0;
             GRANT SELECT ON {name}_nodes, {name}_edges TO {name}_owner;
             SET ROLE {name}_owner;
             SELECT warren.create_graph('{name}_i', 'SELECT id FROM {name}_nodes',
                                        'SELECT src, dst, rel, w FROM {name}_edges',
                                        weighted := true, has_relations := true);
             SELECT warren.create_graph('{name}_l', 'SELECT id FROM {name}_nodes',
                                        'SELECT src, dst, rel, w FROM {name}_edges',
                                        weighted := true, has_relations := true,
                                        node_order := 'locality');
             RESET ROLE;"
        ))?;
        both(name, |graph| refresh(graph, true).map(|_| ()))
    }

    fn both(name: &str, mut each: impl FnMut(&str) -> Result<(), Error>) -> Result<(), Error> {
        for order in ["i", "l"] {
            each(&format!("{name}_{order}"))?;
        }
        Ok(())
    }

    /// Deletes nodes divisible by 17 and adds nodes 121–125 with edges.
    fn change(name: &str) -> Result<(), Error> {
        Spi::run(&format!(
            "DELETE FROM {name}_nodes WHERE id % 17 = 0;
             INSERT INTO {name}_nodes SELECT generate_series(121, 125);
             INSERT INTO {name}_edges SELECT n, n - 100, 'r' || n % 3, 1.5
                 FROM generate_series(121, 125) n"
        ))
    }

    /// The rows of `query` (one text column) on `<name>_i` and `<name>_l`,
    /// with `{g}` replaced by the graph's name, `{v}` by its current version
    /// and `{p}` by the version before: asserted identical, and returned.
    fn same_in_either_order(name: &str, query: &str) -> Result<String, Error> {
        let on = |graph: &str| -> Result<String, Error> {
            let graph = format!("{name}_{graph}");
            let version = current(&graph)?.ok_or(Error::InvalidPosition)?;
            let previous = Spi::get_one::<i64>(&format!(
                "SELECT max(version_id) FROM warren.versions v JOIN warren.graphs g
                 USING (graph_id) WHERE g.name = '{graph}' AND version_id < {version}"
            ))?
            .unwrap_or(version);
            let query = query
                .replace("{g}", &graph)
                .replace("{v}", &version.to_string())
                .replace("{p}", &previous.to_string());
            rows(&query)?.ok_or(Error::InvalidPosition)
        };
        let (by_id, local) = (on("i")?, on("l")?);
        assert_eq!(local, by_id, "{query}");
        assert!(!by_id.is_empty(), "{query} returned no rows");
        Ok(by_id)
    }

    /// Queries whose results, read by external id, must not depend on the
    /// node order.
    const QUERIES: &[&str] = &[
        "SELECT format('%s %s', node_id, depth)
         FROM warren.bfs('{g}', ARRAY[1::bigint], 50, 'both') ORDER BY node_id",
        "SELECT format('%s %s', node_id, depth)
         FROM warren.bfs('{g}', ARRAY[2::bigint, 40], 3, 'in', ARRAY['r1']) ORDER BY node_id",
        "SELECT r::text FROM warren.reachable_set('{g}', ARRAY[3::bigint]) r ORDER BY r",
        "SELECT string_agg(node_id::text, ',' ORDER BY node_id)
         FROM warren.components('{g}', NULL::bigint) GROUP BY component_id
         ORDER BY min(node_id)",
        "SELECT format('%s %s', node_id, round(value::numeric, 9))
         FROM warren.vector_values('{g}', 'rank', NULL::bigint) ORDER BY node_id",
    ];

    fn ranked(name: &str) -> Result<(), Error> {
        both(name, |graph| {
            Spi::run(&format!("SELECT warren.pagerank('{graph}', 'rank', tol := 1e-12)"))
        })
    }

    #[pg_test]
    fn queries_read_the_same_in_either_order() -> Result<(), Error> {
        order_pair("no")?;
        let ids = "SELECT string_agg(dense_id::text, ',' ORDER BY node_id)
                   FROM warren.id_map('{g}', NULL::bigint)";
        assert_ne!(
            rows(&ids.replace("{g}", "no_l"))?,
            rows(&ids.replace("{g}", "no_i"))?,
            "the orders differ"
        );
        ranked("no")?;
        for query in QUERIES {
            same_in_either_order("no", query)?;
        }
        Ok(())
    }

    #[pg_test]
    fn queries_diff_and_distance_read_the_same_across_a_compaction() -> Result<(), Error> {
        order_pair("nc")?;
        change("nc")?;
        both("nc", |graph| refresh(graph, true).map(|_| ()))?;
        both("nc", |graph| Spi::run(&format!("SELECT warren.compact('{graph}')")))?;
        Spi::run(
            "DELETE FROM nc_edges WHERE (src * 3 + dst) % 7 = 0;
             INSERT INTO nc_nodes VALUES (126), (127);
             INSERT INTO nc_edges VALUES (126, 1, 'r0', 2), (127, 126, 'r2', 1), (5, 127, 'r1', 1)",
        )?;
        both("nc", |graph| refresh(graph, true).map(|_| ()))?;
        ranked("nc")?;
        for query in QUERIES {
            same_in_either_order("nc", query)?;
        }
        let diff = same_in_either_order(
            "nc",
            "SELECT (warren.diff('{g}', {p}, {v}) - 'from' - 'to')::text",
        )?;
        assert!(diff.contains("\"removed\""), "{diff}");
        for method in ["edit", "degree"] {
            same_in_either_order(
                "nc",
                &format!("SELECT (warren.distance('{{g}}', {{p}}, {{v}}, '{method}') - 'from' - 'to')::text"),
            )?;
        }
        Ok(())
    }

    /// Later refreshes of a locality graph keep its surviving ids and
    /// append new nodes in id order, as for any graph (DESIGN.md §5.2).
    #[pg_test]
    fn a_refresh_after_a_locality_build_keeps_the_ids() -> Result<(), Error> {
        order_pair("nr")?;
        let ids = "SELECT string_agg(node_id || ':' || dense_id, ' ' ORDER BY node_id)
                   FROM warren.id_map('nr_l', NULL::bigint, {v}) WHERE node_id <= 120";
        let first = current("nr_l")?.ok_or(Error::InvalidPosition)?;
        change("nr")?;
        let second = refresh("nr_l", true)?;
        let at = |version: i64| rows(&ids.replace("{v}", &version.to_string()));
        assert_eq!(at(second)?, at(first)?);
        let appended = rows(
            "SELECT node_id || ':' || dense_id FROM warren.id_map('nr_l', NULL::bigint)
             WHERE node_id > 120 ORDER BY node_id",
        )?;
        assert_eq!(appended.as_deref(), Some("121:120 122:121 123:122 124:123 125:124"));
        Ok(())
    }
}
