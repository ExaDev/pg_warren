//! The community library's SQL surface (DESIGN.md §6.7). Its quality
//! against NetworkX is checked in `test/sql/oracle_louvain.sql`.

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, rows};

    /// A ring of six 5-cliques, each joined to the next by one edge, as
    /// undirected graph `rc` (node ids 1–30; clique c holds 5c+1 … 5c+5).
    fn ring_of_cliques() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE rc_nodes AS SELECT generate_series(1, 30)::bigint AS id;
             CREATE TABLE rc_edges AS
             SELECT a.id AS src, b.id AS dst FROM rc_nodes a JOIN rc_nodes b
               ON (a.id - 1) / 5 = (b.id - 1) / 5 AND a.id < b.id
             UNION ALL SELECT 5 * c + 5, (5 * c + 5) % 30 + 1 FROM generate_series(0, 5) c;
             SELECT warren.create_graph('rc', 'SELECT id FROM rc_nodes', 'SELECT src, dst FROM rc_edges',
                                        directed := false);
             SELECT warren.refresh('rc')",
        )
    }

    /// The communities `name` finds on `rc`, as sorted lists of node ids.
    fn communities(name: &str) -> Result<Option<String>, Error> {
        rows(&format!(
            "SELECT string_agg(node_id::text, ',' ORDER BY node_id)
             FROM warren.vector_values('rc', '{name}', NULL::bigint) GROUP BY value ORDER BY min(node_id)"
        ))
    }

    const CLIQUES: &str =
        "1,2,3,4,5 6,7,8,9,10 11,12,13,14,15 16,17,18,19,20 21,22,23,24,25 26,27,28,29,30";

    #[pg_test]
    fn louvain_and_leiden_find_the_cliques() -> Result<(), Error> {
        ring_of_cliques()?;
        for algorithm in ["louvain", "leiden"] {
            let q =
                Spi::get_one::<f64>(&format!("SELECT warren.{algorithm}('rc', '{algorithm}')"))?;
            assert_eq!(communities(algorithm)?.as_deref(), Some(CLIQUES), "{algorithm}");
            let scored =
                Spi::get_one::<f64>(&format!("SELECT warren.modularity('rc', '{algorithm}')"))?;
            assert_eq!(q, scored, "{algorithm} returns its partition's modularity");
            let labels = rows(&format!(
                "SELECT DISTINCT value::text FROM warren.vector_values('rc', '{algorithm}', NULL::bigint)
                 ORDER BY 1"
            ))?;
            assert_eq!(
                labels.as_deref(),
                Some("0 10 15 20 25 5"),
                "labelled by the least dense id"
            );
        }
        let left = Spi::get_one::<i64>(
            "SELECT count(*) FROM warren.graphs WHERE name LIKE '\\_warren\\_%'",
        )?;
        assert_eq!(left, Some(0), "the levels' graphs are dropped");
        let vectors = rows("SELECT name FROM warren.vectors ORDER BY name")?;
        assert_eq!(vectors.as_deref(), Some("leiden louvain"), "only the outputs persist");
        Ok(())
    }

    /// Leiden aggregates while its refinement merges nodes, as Traag et
    /// al.'s does, rather than stopping at the first level whose local
    /// moving gains nothing: on sparse blocks (20 blocks of 500 nodes,
    /// four edges a node, nine in ten inside the block) that stop left it
    /// below Louvain (0.79 against 0.80 on the bench's larger blocks).
    #[pg_test]
    fn leiden_aggregates_while_its_refinement_merges() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE sb_nodes AS SELECT generate_series(1, 10000)::bigint AS id;
             CREATE TABLE sb_edges AS
             SELECT n.id AS src,
                    CASE WHEN abs(hashint8(n.id * 8 + k)) % 10 < 9
                         THEN (n.id - 1) / 500 * 500 + 1 + abs(hashint8(-(n.id * 8 + k))) % 500
                         ELSE 1 + abs(hashint8(-(n.id * 8 + k))) % 10000 END AS dst
             FROM sb_nodes n, generate_series(1, 4) k;
             SELECT warren.create_graph('sb', 'SELECT id FROM sb_nodes', 'SELECT src, dst FROM sb_edges',
                                        directed := false);
             SELECT warren.refresh('sb')",
        )?;
        let louvain = Spi::get_one::<f64>("SELECT warren.louvain('sb', 'louvain')")?;
        let leiden = Spi::get_one::<f64>("SELECT warren.leiden('sb', 'leiden')")?;
        assert!(leiden > louvain, "leiden {leiden:?}, louvain {louvain:?}");
        Ok(())
    }

    /// The reviewer's two cases: a graph named as a level graph would be
    /// was given a version and then dropped by a run (data loss), and a
    /// reader could create one to make other roles' runs fail.
    #[pg_test]
    fn level_graph_names_can_be_neither_taken_nor_squatted() -> Result<(), Error> {
        ring_of_cliques()?;
        Spi::run("CREATE ROLE squatter; GRANT USAGE ON SCHEMA public TO squatter")?;
        Spi::run("SELECT warren.grant('rc', 'squatter')")?;
        for role in ["NONE", "squatter"] {
            Spi::run(&format!("DROP TABLE IF EXISTS failure; SET ROLE {role}"))?;
            for call in [
                "warren.create_graph('_warren_louvain_' || pg_backend_pid() || '_1',
                                     'SELECT id FROM rc_nodes', 'SELECT src, dst FROM rc_edges')",
                "warren.contract('rc', 'c', '_warren_leiden_' || pg_backend_pid() || '_1')",
                "warren.contract('rc', 'c', '_warren_level_1')",
            ] {
                Spi::run("SELECT warren.vector_dense_id('rc', 'c', persist := false)")?;
                let refused = failure(call)?.unwrap_or_default();
                assert!(
                    refused.starts_with("42939: graph names beginning with _warren_"),
                    "{role}: {refused}"
                );
            }
            Spi::run("DROP TABLE IF EXISTS failure; RESET ROLE")?;
        }
        let graphs = || rows("SELECT name FROM warren.graphs ORDER BY name");
        let before = graphs()?;
        Spi::run("SELECT warren.louvain('rc', 'louvain'), warren.leiden('rc', 'leiden')")?;
        assert_eq!(graphs()?, before, "a run leaves no graph and adds none");
        Ok(())
    }

    /// Every community Leiden returns is connected, whichever way it stops:
    /// after one level (`max_levels`), it returns the refined partition,
    /// which is connected, not the moved one, which need not be.
    #[pg_test]
    fn leiden_communities_are_connected_however_it_stops() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE sp_nodes AS SELECT generate_series(1, 3000)::bigint AS id;
             CREATE TABLE sp_edges AS
             SELECT n.id AS src, (1 + abs(hashint8(n.id * 4 + k)) % 3000)::bigint AS dst
             FROM sp_nodes n, generate_series(1, 2) k;
             SELECT warren.create_graph('sp', 'SELECT id FROM sp_nodes', 'SELECT src, dst FROM sp_edges',
                                        directed := false);
             SELECT warren.refresh('sp')",
        )?;
        // After one level it is the refinement of that level's moving.
        Spi::run(
            "SELECT warren.leiden('sp', 'one_level', max_levels := 1);
             SELECT warren.local_moving('sp', 'moved');
             SELECT warren.leiden_refine('sp', 'moved', 'refined')",
        )?;
        let differ = Spi::get_one::<i64>(
            "SELECT count(*) FROM warren.vector_values('sp', 'one_level', NULL::bigint) a
             JOIN warren.vector_values('sp', 'refined', NULL::bigint) b USING (node_id)
             WHERE a.value <> b.value",
        )?;
        assert_eq!(differ, Some(0), "max_levels := 1 returns the refined partition");
        for (call, out) in [
            ("max_levels := 1", "one"),
            ("max_levels := 2", "two"),
            ("seed := 3, max_levels := 1", "three"),
            ("resolution := 0.2, max_levels := 1", "coarse"),
            ("", "all"),
        ] {
            let sep = if call.is_empty() { "" } else { ", " };
            Spi::run(&format!("SELECT warren.leiden('sp', '{out}'{sep}{call})"))?;
            let split = Spi::get_one::<i64>(&format!(
                "WITH l AS (SELECT node_id, value FROM warren.vector_values('sp', '{out}', NULL::bigint)),
                      inside AS (SELECT e.src, e.dst FROM sp_edges e JOIN l a ON a.node_id = e.src
                                 JOIN l b ON b.node_id = e.dst WHERE a.value = b.value)
                 SELECT count(*) FROM l WHERE node_id NOT IN (SELECT src FROM inside UNION SELECT dst FROM inside)
                   AND (SELECT count(*) FROM l l2 WHERE l2.value = l.value) > 1"
            ))?;
            assert_eq!(split, Some(0), "{call}: members with no edge inside their community");
            Spi::run(&format!(
                "DROP TABLE IF EXISTS inside_{out};
                 CREATE TABLE inside_{out} AS
                 SELECT e.src, e.dst FROM sp_edges e
                 JOIN warren.vector_values('sp', '{out}', NULL::bigint) a ON a.node_id = e.src
                 JOIN warren.vector_values('sp', '{out}', NULL::bigint) b ON b.node_id = e.dst
                 WHERE a.value = b.value;
                 SELECT warren.create_graph('inside_{out}', 'SELECT id FROM sp_nodes',
                                            'SELECT src, dst FROM inside_{out}', directed := false);
                 SELECT warren.refresh('inside_{out}')"
            ))?;
            let components = Spi::get_one::<i64>(&format!(
                "SELECT count(DISTINCT component_id) FROM warren.components('inside_{out}', NULL::bigint)"
            ))?;
            let communities = Spi::get_one::<i64>(&format!(
                "SELECT count(DISTINCT value) FROM warren.vector_values('sp', '{out}', NULL::bigint)"
            ))?;
            assert_eq!(components, communities, "{call}: every community is connected");
        }
        Ok(())
    }

    #[pg_test]
    fn each_levels_partition_can_be_kept() -> Result<(), Error> {
        ring_of_cliques()?;
        Spi::run("SELECT warren.louvain('rc', 'c', levels_prefix := 'level')")?;
        let levels =
            rows("SELECT name FROM warren.vectors WHERE name LIKE 'level%' ORDER BY name")?;
        assert!(levels.as_deref().is_some_and(|l| l.starts_with("level_1")), "{levels:?}");
        Ok(())
    }

    #[pg_test]
    fn a_seed_gives_the_same_result_every_time() -> Result<(), Error> {
        ring_of_cliques()?;
        Spi::run(
            "SELECT warren.leiden('rc', 'a', seed := 7, resolution := 3);
             SELECT warren.leiden('rc', 'b', seed := 7, resolution := 3)",
        )?;
        let differ = Spi::get_one::<i64>(
            "SELECT count(*) FROM warren.vector_values('rc', 'a', NULL::bigint) a
             JOIN warren.vector_values('rc', 'b', NULL::bigint) b USING (node_id) WHERE a.value <> b.value",
        )?;
        assert_eq!(differ, Some(0));
        Ok(())
    }

    #[pg_test]
    fn a_graph_without_edges_keeps_every_node_alone() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE lone_nodes AS SELECT generate_series(1, 3)::bigint AS id;
             CREATE TABLE lone_edges (src bigint, dst bigint);
             SELECT warren.create_graph('lone', 'SELECT id FROM lone_nodes', 'SELECT src, dst FROM lone_edges');
             SELECT warren.refresh('lone')",
        )?;
        let q = Spi::get_one::<f64>("SELECT warren.louvain('lone', 'c')")?;
        assert_eq!(q, Some(0.0));
        let labels =
            rows("SELECT value::text FROM warren.vector_values('lone', 'c', NULL::bigint)")?;
        assert_eq!(labels.as_deref(), Some("0 1 2"));
        Ok(())
    }

    #[pg_test]
    fn the_library_refuses_bad_arguments() -> Result<(), Error> {
        ring_of_cliques()?;
        assert_eq!(
            failure("warren.louvain('rc', NULL)")?.as_deref(),
            Some("22004: out must not be NULL")
        );
        assert_eq!(
            failure("warren.leiden('rc', 'c', max_levels := 0)")?.as_deref(),
            Some("22023: max_levels must be at least 1, not 0")
        );
        assert_eq!(
            failure("warren.leiden('rc', '_warren_c')")?.as_deref(),
            Some("42939: vector names beginning with _warren_ are reserved for the library's intermediates")
        );
        for algorithm in ["louvain", "leiden"] {
            for (tolerance, shown) in [("-1", "-1"), ("'NaN'", "NaN")] {
                assert_eq!(
                    failure(&format!("warren.{algorithm}('rc', 'c', tolerance := {tolerance})"))?,
                    Some(format!("22023: tolerance must be a number of at least 0, not {shown}")),
                    "{algorithm}"
                );
            }
        }
        Ok(())
    }
}
