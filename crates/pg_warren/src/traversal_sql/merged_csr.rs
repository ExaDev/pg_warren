//! Tests: every traversal function reading all relations returns the same
//! rows, in the same order, with the same work counts in `last_stats()`,
//! whether the graph stores its merged CSR or merges the relations' rows
//! on the fly (DESIGN.md §5.3).

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{merged_pair, same_with_or_without_merged};

    /// `last_stats()` after running `query`, without the graph and version
    /// it names.
    const STATS_AFTER: &str = "CREATE FUNCTION pg_temp.stats_after(query text) RETURNS text
        LANGUAGE plpgsql AS $f$
        BEGIN
            EXECUTE query;
            RETURN (warren.last_stats() - 'graph' - 'version')::text;
        END $f$";

    fn same(query: &str) -> Result<String, Error> {
        same_with_or_without_merged("tm", query)
    }

    #[pg_test]
    fn traversals_read_the_same_without_the_merged_csr() -> Result<(), Error> {
        merged_pair("tm")?;
        Spi::run(STATS_AFTER)?;
        let ids = "generate_series(1::bigint, 125) id";
        for direction in ["out", "in", "both"] {
            let d = format!("direction := '{direction}'");
            let bfs = format!("warren.bfs('{{g}}', ARRAY[1::bigint, 50, 121], 4, {d})");
            same(&format!("SELECT format('%s %s %s', node_id, depth, via) FROM {bfs}"))?;
            same(&format!("SELECT pg_temp.stats_after($q$SELECT count(*) FROM {bfs}$q$)"))?;
            let reachable = format!("warren.reachable_set('{{g}}', ARRAY[3::bigint], {d})");
            same(&format!("SELECT n::text FROM {reachable} n"))?;
            same(&format!("SELECT pg_temp.stats_after($q$SELECT count(*) FROM {reachable}$q$)"))?;
            same(&format!(
                "SELECT id || ':' || (SELECT coalesce(string_agg(n::text, ','), '')
                                       FROM warren.neighbours('{{g}}', id, '{direction}') n)
                 FROM {ids}"
            ))?;
            same(&format!("SELECT warren.degree('{{g}}', id, '{direction}')::text FROM {ids}"))?;
            same(&format!(
                "SELECT warren.reachable('{{g}}', 2::bigint, id, 3, {d})::text FROM {ids}"
            ))?;
            same(&format!(
                "SELECT coalesce(warren.shortest_path('{{g}}', 1::bigint, id, {d})::text, '-')
                 FROM {ids}"
            ))?;
            same(&format!(
                "SELECT coalesce((SELECT format('%s %s', path, cost)
                                  FROM warren.dijkstra('{{g}}', 2::bigint, id, {d})), '-')
                 FROM {ids}"
            ))?;
        }
        same(&format!(
            "SELECT count(*)::text FROM generate_series(1::bigint, 60) s, {ids}
             WHERE warren.has_edge('{{g}}', s, id)"
        ))?;
        let components = "warren.components('{g}', NULL::bigint)";
        same(&format!("SELECT format('%s %s', node_id, component_id) FROM {components}"))?;
        same(&format!("SELECT pg_temp.stats_after($q$SELECT count(*) FROM {components}$q$)"))?;
        let steps = "ARRAY[warren.s(NULL, 'out', NULL, 1, 3), warren.s(NULL, 'both')]";
        for mode in ["ends", "paths", "count"] {
            for options in ["", ", distinct_ends := false", ", reverse := true"] {
                let walk = format!(
                    "warren.walk('{{g}}', ARRAY[1::bigint, 7, 121], {steps}, mode := '{mode}',
                                 max_results := 2000{options})"
                );
                same(&format!(
                    "SELECT format('%s %s %s %s', end_id, path, rels, count) FROM {walk}"
                ))?;
                same(&format!("SELECT pg_temp.stats_after($q$SELECT count(*) FROM {walk}$q$)"))?;
            }
        }
        Ok(())
    }
}
