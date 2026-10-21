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
        Ok(())
    }
}
