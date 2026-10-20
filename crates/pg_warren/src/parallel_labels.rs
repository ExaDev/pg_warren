//! The parallel-safety label of every `warren.*` function (DESIGN.md §6.9,
//! §9.3), and checks that the functions that read run in parallel mode.
//!
//! Functions that write, or may persist what they compute, are `PARALLEL
//! UNSAFE`. Functions that read a graph or vector are `PARALLEL
//! RESTRICTED`: the backend-local cache and transient vectors live only in
//! the leader, so a parallel worker would cold-load the version (or not see
//! the vector at all). Only functions that read no graph are `PARALLEL
//! SAFE`. Restricted functions still run in the leader while a parallel
//! plan is executing, which is parallel mode: there they may not assign a
//! transaction id or write anything.

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{refresh, ring};

    /// Every `warren.*` function and its `proparallel`: `u` unsafe, `r`
    /// restricted, `s` safe.
    const LABELS: &[(&str, &str)] = &[
        ("aggregate_neighbours", "u"),
        ("bfs", "r"),
        ("compact", "u"),
        ("components", "r"),
        ("components_vector", "u"),
        ("create_graph", "u"),
        ("current_version_is_kept", "u"),
        ("current_version_is_the_graphs", "u"),
        ("degree", "r"),
        ("degree_vector", "u"),
        ("diff", "r"),
        ("dijkstra", "r"),
        ("distance", "r"),
        ("drop_graph", "u"),
        ("drop_vector", "u"),
        ("drop_version", "u"),
        ("eigen", "u"),
        ("evict", "r"),
        ("explain_map", "r"),
        ("gather", "u"),
        ("grant", "u"),
        ("has_edge", "r"),
        ("id_map", "r"),
        ("info", "r"),
        ("label_propagation", "u"),
        ("last_stats", "r"),
        ("matrix_rows", "r"),
        ("neighbours", "r"),
        ("pagerank", "u"),
        ("prewarm", "r"),
        ("promote", "u"),
        ("reachable", "r"),
        ("reachable_set", "r"),
        ("refresh", "u"),
        ("revoke", "u"),
        ("s", "s"),
        ("shortest_path", "r"),
        ("spmm", "u"),
        ("spmv", "u"),
        ("sssp_vector", "u"),
        ("steps", "s"),
        ("triangles", "u"),
        ("vector_const", "u"),
        ("vector_degree", "u"),
        ("vector_dense_id", "u"),
        ("vector_from_query", "u"),
        ("vector_map", "u"),
        ("vector_norm_diff", "r"),
        ("vector_reduce", "r"),
        ("vector_stats", "r"),
        ("vector_topk", "r"),
        ("vector_values", "r"),
        ("version", "s"),
        ("walk", "r"),
    ];

    /// A call of every function that is not `PARALLEL UNSAFE`, on graph
    /// `pp` with vector `c` and eigenvector `lap_1`; `{v}` is its current
    /// version, `{w}` that of graph `pq`.
    const READS: &[&str] = &[
        // Transient vectors live only in the leader's backend (and before
        // `evict`, which frees them).
        "warren.vector_reduce('pp', 't', 'sum')",
        "warren.vector_values('pp', 't', NULL::bigint)",
        "warren.bfs('pp', ARRAY[1::bigint], 2)",
        "warren.components('pp', NULL::bigint)",
        "warren.degree('pp', 1::bigint)",
        "warren.diff('pp', {v}, {v})",
        "warren.dijkstra('pp', 1::bigint, 2::bigint)",
        "warren.distance('pp', {v}, {v}, 'edit')",
        "warren.distance('pp', {v}, {v}, 'degree')",
        "warren.distance('pp', {v}, {v}, 'spectral', k := 1)",
        "warren.distance('pp', {v}, {v}, 'embedding', x := 'c', compared_rows := 'all')",
        "warren.distance('pp', {v}, {v}, 'subspace', vectors_prefix := 'lap', k := 1)",
        "warren.distance('pp', {v}, {v}, 'spectral', vectors_prefix := 'lap', k := 1, matrix := 'laplacian')",
        "warren.distance('pp', {v}, {w}, 'subspace', vectors_prefix := 'lap', k := 1, graph2 := 'pq')",
        "warren.distance('pp', {v}, {w}, 'edit', graph2 := 'pq')",
        "warren.evict('pp')",
        "warren.explain_map('pp', 'c + 1')",
        "warren.has_edge('pp', 1::bigint, 2::bigint)",
        "warren.id_map('pp', NULL::bigint)",
        "warren.info('pp')",
        "warren.last_stats()",
        "warren.matrix_rows('pp', 'c', NULL::bigint)",
        "warren.neighbours('pp', 1::bigint)",
        "warren.prewarm('pp')",
        "warren.reachable('pp', 1::bigint, 2::bigint)",
        "warren.reachable_set('pp', ARRAY[1::bigint])",
        "warren.s(NULL)",
        "warren.shortest_path('pp', 1::bigint, 2::bigint)",
        "warren.steps('[]')",
        "warren.vector_norm_diff('pp', 'c', 'c')",
        "warren.vector_reduce('pp', 'c', 'sum')",
        "warren.vector_stats('pp', 'c')",
        "warren.vector_topk('pp', 'c', 3, NULL::bigint)",
        "warren.vector_values('pp', 'c', NULL::bigint)",
        "warren.version()",
        "warren.walk('pp', ARRAY[1::bigint], ARRAY[warren.s(NULL)])",
    ];

    /// Graph `pp` (a ring of 5) with a persisted vector `c` and a transient
    /// vector `t`; returns its current version.
    fn graph_with_a_vector() -> Result<i64, Error> {
        ring("pp", 5, 2)?;
        let version = refresh("pp", true)?;
        Spi::run(
            "SELECT warren.vector_const('pp', 'c', 1);
             SELECT warren.vector_const('pp', 't', 2, persist := false)",
        )?;
        Ok(version)
    }

    /// Eigenvectors `lap_1` on `pp`'s current version and on graph `pq` (a
    /// ring of 5 too), for the spectral reads; returns `pq`'s version.
    fn eigenvectors_and_another_graph() -> Result<i64, Error> {
        ring("pq", 5, 2)?;
        let other = refresh("pq", true)?;
        Spi::run(
            "SELECT * FROM warren.eigen('pp', 1, 'laplacian', vectors_prefix := 'lap');
             SELECT * FROM warren.eigen('pq', 1, 'laplacian', vectors_prefix := 'lap')",
        )?;
        Ok(other)
    }

    /// Runs `PERFORM <query>` in a subtransaction, which has no
    /// transaction id of its own until something assigns one: in parallel
    /// mode, assigning one raises "cannot assign XIDs during a parallel
    /// operation".
    fn in_subtransaction(query: &str) -> Result<(), Error> {
        Spi::run(&format!(
            "DO $$ BEGIN
                 BEGIN
                     PERFORM {query};
                 EXCEPTION WHEN division_by_zero THEN NULL;
                 END;
             END $$"
        ))
    }

    #[pg_test]
    fn every_function_has_its_declared_parallel_label() -> Result<(), Error> {
        let found = Spi::get_one::<String>(
            "SELECT string_agg(DISTINCT proname || ':' || proparallel::text, ',' ORDER BY
                               proname || ':' || proparallel::text)
             FROM pg_proc WHERE pronamespace = 'warren'::regnamespace",
        )?;
        let declared: Vec<String> =
            LABELS.iter().map(|(name, label)| format!("{name}:{label}")).collect();
        assert_eq!(found, Some(declared.join(",")));
        Ok(())
    }

    /// The setting that forces parallel mode; PG 16 renamed it.
    #[cfg(feature = "pg15")]
    const FORCE_PARALLEL: &str = "force_parallel_mode";
    #[cfg(not(feature = "pg15"))]
    const FORCE_PARALLEL: &str = "debug_parallel_query";

    #[pg_test]
    fn every_read_runs_in_parallel_mode() -> Result<(), Error> {
        let version = graph_with_a_vector()?;
        let other = eigenvectors_and_another_graph()?;
        Spi::run(&format!("SET {FORCE_PARALLEL} = on"))?;
        for read in READS {
            let read = read.replace("{v}", &version.to_string()).replace("{w}", &other.to_string());
            // Without a transaction id: assigning one would fail.
            in_subtransaction(&read)?;
            // In the test's transaction, which holds one: SPI then reads
            // non-read-only (a command counter increment, a new snapshot).
            Spi::run(&format!("SELECT {read}"))?;
        }
        Spi::run(&format!("RESET {FORCE_PARALLEL}"))
    }

    #[test]
    fn every_function_that_is_not_unsafe_has_a_read() {
        for (name, label) in LABELS.iter().filter(|(_, label)| *label != "u") {
            let call = format!("warren.{name}(");
            assert!(
                READS.iter().any(|read| read.starts_with(&call)),
                "{name} ({label}) has no read"
            );
        }
    }

    #[pg_test]
    fn a_read_in_the_init_plan_of_a_parallel_scan_runs() -> Result<(), Error> {
        graph_with_a_vector()?;
        Spi::run(
            "CREATE TABLE big AS SELECT generate_series(1, 100000) AS g; ANALYZE big;
             SET parallel_setup_cost = 0; SET parallel_tuple_cost = 0;
             SET min_parallel_table_scan_size = 0",
        )?;
        let plan = Spi::connect(|client| {
            client
                .select(
                    "EXPLAIN SELECT count(*) FROM big
                     WHERE g < (SELECT warren.vector_reduce('pp', 'c', 'sum'))",
                    None,
                    &[],
                )?
                .map(|row| row.get::<String>(1).map(Option::unwrap_or_default))
                .collect::<Result<Vec<_>, _>>()
        })?;
        assert!(plan.iter().any(|line| line.contains("Gather")), "{plan:?}");
        in_subtransaction(
            "count(*) FROM big WHERE g < (SELECT warren.vector_reduce('pp', 'c', 'sum'))",
        )?;
        Spi::run("RESET parallel_setup_cost; RESET parallel_tuple_cost; RESET min_parallel_table_scan_size")
    }
}
