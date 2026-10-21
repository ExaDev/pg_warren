//! In-database tests of the algorithm library, and the helpers the
//! library's other test modules share.

use pgrx::prelude::*;
use pgrx::spi::Error;

use super::super::map::executor_maps;
use crate::fixture::{failure, rows};

/// `name`'s values, as `node:value` text, rounded to `digits`.
pub(super) fn values(graph: &str, name: &str, digits: i32) -> Result<Option<String>, Error> {
    rows(&format!(
        "SELECT node_id || ':' || round(value::numeric, {digits})
         FROM warren.vector_values('{graph}', '{name}', NULL::bigint)"
    ))
}

/// Runs `sql` and checks that every vector_map it made was compiled and
/// that it left no intermediate vector of `algorithm` behind.
pub(super) fn compiled(graph: &str, algorithm: &str, sql: &str) -> Result<(), Error> {
    let before = executor_maps();
    Spi::run(sql)?;
    assert_eq!(executor_maps(), before, "{algorithm}: a vector_map took the executor path");
    let left = failure(&format!("warren.explain_map('{graph}', '_warren_{algorithm}_x')"))?;
    assert!(left.is_some(), "{algorithm} left its intermediates");
    Ok(())
}

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::{compiled, values};
    use crate::fixture::{failure, refresh, ring, rows, small};

    #[pg_test]
    fn pagerank_is_uniform_on_a_ring_and_personalised_by_a_seed() -> Result<(), Error> {
        ring("r", 4, 2)?;
        refresh("r", true)?;
        compiled("r", "pagerank", "SELECT warren.pagerank('r', 'pr', tol := 1e-12)")?;
        assert_eq!(
            values("r", "pr", 6)?.as_deref(),
            Some("1:0.250000 2:0.250000 3:0.250000 4:0.250000")
        );
        Spi::run("SELECT warren.vector_from_query('r', 'seed', 'SELECT 1::bigint, 1')")?;
        compiled(
            "r",
            "pagerank",
            "SELECT warren.pagerank('r', 'ppr', seed := 'seed', tol := 1e-12)",
        )?;
        let sum = Spi::get_one::<f64>("SELECT warren.vector_reduce('r', 'ppr', 'sum')")?
            .unwrap_or_default();
        assert!((sum - 1.0).abs() < 1e-9, "{sum}");
        let top =
            rows("SELECT node_id::text FROM warren.vector_topk('r', 'ppr', 1, NULL::bigint)")?;
        assert_eq!(top.as_deref(), Some("1"));
        Ok(())
    }

    #[pg_test]
    fn pagerank_does_not_depend_on_extra_float_digits() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "SELECT warren.vector_from_query('t', 'seed', 'SELECT id, id / 3.0 FROM t_nodes');
             SELECT warren.pagerank('t', 'pr', seed := 'seed', tol := 1e-12);
             SET LOCAL extra_float_digits = -10;
             SELECT warren.pagerank('t', 'pr_short', seed := 'seed', tol := 1e-12);
             RESET extra_float_digits",
        )?;
        let differ = Spi::get_one::<i64>(
            "SELECT count(*) FROM warren.vector_values('t', 'pr', NULL::bigint) a
             JOIN warren.vector_values('t', 'pr_short', NULL::bigint) b USING (node_id)
             WHERE a.value <> b.value",
        )?;
        assert_eq!(differ, Some(0));
        Ok(())
    }

    #[pg_test]
    fn a_failed_run_drops_its_intermediates() -> Result<(), Error> {
        small("t")?;
        let failed = failure("warren.pagerank('t', 'pr', seed := 'nope')")?.unwrap_or_default();
        assert!(failed.starts_with("42704: graph \"t\" has no vector \"nope\""), "{failed}");
        for part in ["one", "out", "x"] {
            let left = failure(&format!("warren.explain_map('t', '_warren_pagerank_{part}')"))?;
            assert!(left.is_some(), "_warren_pagerank_{part} was left behind");
        }
        Ok(())
    }

    #[pg_test]
    fn library_names_are_reserved() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT warren.vector_const('t', 'e', 1)")?;
        let calls = [
            "warren.pagerank('t', '_warren_pagerank_x')",
            "warren.pagerank('t', 'pr', seed := '_warren_seed')",
            "warren.components_vector('t', '_warren_cc')",
            "warren.label_propagation('t', 'lp', mask := '_warren_m')",
            "warren.label_propagation('t', 'lp', seed := '_warren_s')",
            "warren.sssp_vector('t', '_warren_d', 1::bigint)",
            "warren.degree_vector('t', '_warren_deg')",
            "warren.aggregate_neighbours('t', '_warren_x', 'a')",
            "warren.aggregate_neighbours('t', 'e', '_warren_a')",
            "warren.eigenvector_centrality('t', '_warren_ev')",
            "warren.katz_centrality('t', '_warren_k')",
            "warren.katz_centrality('t', 'k', seed := '_warren_s')",
            "warren.hits('t', '_warren_h', 'a')",
            "warren.hits('t', 'h', '_warren_a')",
            "warren.clustering('t', '_warren_c')",
            "warren.label_propagation_majority('t', '_warren_lp')",
            "warren.label_propagation_majority('t', 'lp', seed := '_warren_s')",
        ];
        for call in calls {
            let refused = failure(call)?.unwrap_or_default();
            assert!(
                refused.starts_with("42939: vector names beginning with _warren_ are reserved"),
                "{call}: {refused}"
            );
        }
        Ok(())
    }

    #[pg_test]
    fn pagerank_returns_its_iterations_and_stops_at_max_iter() -> Result<(), Error> {
        small("t")?;
        let iterations =
            Spi::get_one::<i32>("SELECT warren.pagerank('t', 'pr', max_iter := 3, tol := 0)")?;
        assert_eq!(iterations, Some(3));
        let sum = Spi::get_one::<f64>("SELECT warren.vector_reduce('t', 'pr', 'sum')")?
            .unwrap_or_default();
        assert!((sum - 1.0).abs() < 1e-9, "dangling node 6's rank is redistributed: {sum}");
        Ok(())
    }

    #[pg_test]
    fn degree_vector_counts_distinct_neighbours() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT warren.degree_vector('t', 'deg'); SELECT warren.degree_vector('t', 'in_a', 'in', ARRAY['a'])")?;
        assert_eq!(values("t", "deg", 0)?.as_deref(), Some("1:2 2:1 3:1 4:1 5:1 6:0"));
        assert_eq!(values("t", "in_a", 0)?.as_deref(), Some("1:1 2:1 3:1 4:0 5:0 6:0"));
        Ok(())
    }

    #[pg_test]
    fn components_vector_labels_weak_components_as_components_does() -> Result<(), Error> {
        small("t")?;
        compiled("t", "components_vector", "SELECT warren.components_vector('t', 'cc')")?;
        let differs = rows(
            "SELECT c.node_id::text FROM warren.components('t', NULL::bigint) c
             JOIN warren.vector_values('t', 'cc', NULL::bigint) v USING (node_id)
             WHERE v.value <> c.component_id",
        )?;
        assert_eq!(differs.as_deref(), Some(""));
        Ok(())
    }

    #[pg_test]
    fn label_propagation_spreads_the_smallest_label_within_the_mask() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT warren.vector_from_query('t', 'keep', 'SELECT id, id <> 2 FROM t_nodes', 'bool')")?;
        compiled(
            "t",
            "label_propagation",
            "SELECT warren.label_propagation('t', 'lp', mask := 'keep')",
        )?;
        assert_eq!(values("t", "lp", 0)?.as_deref(), Some("1:0 2:1 3:0 4:0 5:4 6:5"));
        Spi::run(
            "SELECT warren.vector_from_query('t', 'seed', 'SELECT id, 10 - id FROM t_nodes')",
        )?;
        compiled(
            "t",
            "label_propagation",
            "SELECT warren.label_propagation('t', 'lp', seed := 'seed', max_iter := 1)",
        )?;
        assert_eq!(values("t", "lp", 0)?.as_deref(), Some("1:6 2:7 3:6 4:6 5:5 6:4"));
        Ok(())
    }

    #[pg_test]
    fn sssp_vector_matches_dijkstra() -> Result<(), Error> {
        small("t")?;
        compiled("t", "sssp_vector", "SELECT warren.sssp_vector('t', 'd', 1::bigint)")?;
        assert_eq!(values("t", "d", 0)?.as_deref(), Some("1:0 2:1 3:2 4:3 5:Infinity 6:Infinity"));
        let differs = rows(
            "SELECT v.node_id::text FROM warren.vector_values('t', 'd', NULL::bigint) v
             LEFT JOIN LATERAL warren.dijkstra('t', 1::bigint, v.node_id) p ON true
             WHERE v.value IS DISTINCT FROM coalesce(p.cost, 'Infinity')",
        )?;
        assert_eq!(differs.as_deref(), Some(""));
        Spi::run("SELECT warren.sssp_vector('t', 'hops', 1::bigint, weighted := false)")?;
        assert_eq!(
            values("t", "hops", 0)?.as_deref(),
            Some("1:0 2:1 3:2 4:1 5:Infinity 6:Infinity")
        );
        Spi::run(
            "CREATE TABLE neg_nodes (id bigint); INSERT INTO neg_nodes VALUES (1), (2), (3), (4);
             CREATE TABLE neg_edges (src bigint, dst bigint, w float8);
             INSERT INTO neg_edges VALUES (1, 2, 1), (2, 3, -1), (3, 4, -1), (4, 2, -1);
             SELECT warren.create_graph('neg', 'SELECT id FROM neg_nodes', 'SELECT src, dst, w FROM neg_edges',
                                        weighted := true);
             SELECT warren.refresh('neg')",
        )?;
        let cycle = failure("warren.sssp_vector('neg', 'd', 1::bigint)")?.unwrap_or_default();
        assert!(cycle.starts_with("22023: a negative cycle is reachable from node 1"), "{cycle}");
        Spi::run("SELECT warren.sssp_vector('neg', 'd', 1::bigint, max_iter := 2)")?;
        let unknown = failure("warren.sssp_vector('t', 'd', 99::bigint)")?.unwrap_or_default();
        assert!(unknown.starts_with("22023: node 99 is not a node of graph \"t\""), "{unknown}");
        Ok(())
    }

    #[pg_test]
    fn aggregate_neighbours_averages_sums_or_maxes_neighbour_rows() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT warren.vector_from_query('t', 'e', 'SELECT id, ARRAY[id, 10 * id] FROM t_nodes', 'f32')")?;
        let matrix = |name: &str| {
            rows(&format!("SELECT node_id || ':' || row::text FROM warren.matrix_rows('t', '{name}', NULL::bigint)"))
        };
        compiled(
            "t",
            "aggregate_neighbours",
            "SELECT warren.aggregate_neighbours('t', 'e', 'mean')",
        )?;
        assert_eq!(
            matrix("mean")?.as_deref(),
            Some("1:{3,30} 2:{3,30} 3:{4,40} 4:{1,10} 5:{5,50} 6:{NaN,NaN}")
        );
        Spi::run(
            "SELECT warren.aggregate_neighbours('t', 'e', 'sum', 'sum');
             SELECT warren.aggregate_neighbours('t', 'e', 'max', 'max', rels := ARRAY['a']);
             SELECT warren.aggregate_neighbours('t', 'e', 'two', 'sum', hops := 2, direction := 'in')",
        )?;
        assert_eq!(
            matrix("sum")?.as_deref(),
            Some("1:{6,60} 2:{3,30} 3:{4,40} 4:{1,10} 5:{5,50} 6:{0,0}")
        );
        assert_eq!(
            matrix("max")?.as_deref(),
            Some("1:{2,20} 2:{3,30} 3:{-Infinity,-Infinity} 4:{1,10} 5:{-Infinity,-Infinity} 6:{-Infinity,-Infinity}")
        );
        // In-neighbours of in-neighbours: 1 ← 4 ← {1, 3}; 2 ← 1 ← 4; …
        assert_eq!(
            matrix("two")?.as_deref(),
            Some("1:{4,40} 2:{4,40} 3:{1,10} 4:{6,60} 5:{5,50} 6:{0,0}")
        );
        let bad =
            failure("warren.aggregate_neighbours('t', 'e', 'x', 'median')")?.unwrap_or_default();
        assert!(bad.starts_with("22023: agg must be mean, sum or max"), "{bad}");
        Ok(())
    }

    #[pg_test]
    fn bad_arguments_are_refused() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "CREATE TABLE none_nodes (id bigint);
             SELECT warren.create_graph('none', 'SELECT id FROM none_nodes', 'SELECT id, id FROM none_nodes');
             SELECT warren.refresh('none')",
        )?;
        let calls = [
            ("warren.eigenvector_centrality('nope', 'ev')", "42704: graph \"nope\" does not exist"),
            (
                "warren.eigenvector_centrality('t', 'ev', tol := -1)",
                "22023: tol must be at least 0, not -1",
            ),
            (
                "warren.eigenvector_centrality('t', 'ev', max_iter := 0)",
                "22023: max_iter must be at least 1, not 0",
            ),
            (
                "warren.eigenvector_centrality('none', 'ev')",
                "22023: graph \"none\" has no live nodes",
            ),
        ];
        for (call, expected) in calls {
            let refused = failure(call)?.unwrap_or_default();
            assert!(refused.starts_with(expected), "{call}: {refused}");
        }
        Ok(())
    }

    /// Each function of DESIGN.md §6.7's group 1 and its intermediates.
    const GROUP_ONE: &[(&str, &str, &[&str])] = &[
        (
            "eigenvector_centrality",
            "warren.eigenvector_centrality('r', 'taken')",
            &["one", "x", "y", "next", "scale"],
        ),
        (
            "katz_centrality",
            "warren.katz_centrality('r', 'taken')",
            &["one", "b", "alpha", "x", "y", "next", "scale"],
        ),
        ("hits", "warren.hits('r', 'h', 'taken')", &["one", "x", "a", "next", "scale"]),
        ("clustering", "warren.clustering('r', 'taken')", &["triangles", "degree"]),
        (
            "label_propagation_majority",
            "warren.label_propagation_majority('r', 'taken')",
            &["x", "next"],
        ),
    ];

    /// A reader may not replace the owner's vector, so writing `out` fails
    /// after every intermediate exists: the run drops them all.
    #[pg_test]
    fn a_run_that_cannot_write_its_output_drops_its_intermediates() -> Result<(), Error> {
        ring("r", 5, 2)?;
        refresh("r", true)?;
        Spi::run(
            "CREATE ROLE r_reader;
             SELECT warren.grant('r', 'r_reader');
             SET ROLE r_owner;
             SELECT warren.vector_const('r', 'taken', 1);
             SET ROLE r_reader",
        )?;
        for (algorithm, call, parts) in GROUP_ONE {
            let refused = failure(call)?.unwrap_or_default();
            assert!(refused.starts_with("42501: "), "{call}: {refused}");
            for part in *parts {
                let left =
                    failure(&format!("warren.explain_map('r', '_warren_{algorithm}_{part}')"))?;
                assert!(left.is_some(), "_warren_{algorithm}_{part} was left behind");
            }
        }
        Spi::run("RESET ROLE")
    }

    /// Constant vectors hold zero at tombstoned rows, and `vector_map`
    /// evaluates those rows too, so the library never divides by one.
    #[pg_test]
    fn the_library_runs_on_a_version_with_tombstones() -> Result<(), Error> {
        ring("r", 6, 2)?;
        refresh("r", true)?;
        Spi::run("DELETE FROM r_nodes WHERE id = 3")?;
        refresh("r", true)?;
        Spi::run(
            "SELECT warren.vector_from_query('r', 'seed', 'SELECT id, id FROM r_nodes');
             SELECT warren.pagerank('r', 'pr', seed := 'seed');
             SELECT warren.eigenvector_centrality('r', 'ev');
             SELECT warren.katz_centrality('r', 'k', seed := 'seed');
             SELECT warren.hits('r', 'h', 'a');
             SELECT warren.clustering('r', 'cl');
             SELECT warren.label_propagation_majority('r', 'lpm', seed := 'seed')",
        )?;
        // The ring is now the path 4 → 5 → 6 → 1 → 2: every node with an
        // out-edge is an equal hub.
        assert_eq!(
            values("r", "h", 3)?.as_deref(),
            Some("1:0.250 2:0.000 4:0.250 5:0.250 6:0.250")
        );
        Ok(())
    }
}
