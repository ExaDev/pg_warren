//! The parallel-safety label of every `warren.*` function (DESIGN.md §6.9,
//! §9.3, §14 item 33), and checks that the functions that read run in
//! parallel mode, and the safe ones in parallel workers.
//!
//! Functions that write, or may persist what they compute, are `PARALLEL
//! UNSAFE`. Graph reads that use no vector (`neighbours`, `degree`,
//! `has_edge`, `id_map`, `info` and the traversals) are `PARALLEL SAFE`: a
//! worker maps the shared version, or loads its own copy where the shared
//! cache is unavailable, and a traversal it runs records no
//! `last_stats()`. The other reads are `PARALLEL RESTRICTED`: transient
//! vectors live only in the leader's backend, and `last_stats()`,
//! `prewarm` and `evict` are about the leader's session. Functions that
//! read no graph are `PARALLEL SAFE`. Restricted functions still run in
//! the leader while a parallel plan is executing, which is parallel mode:
//! there, as in a worker, they may not assign a transaction id or write
//! anything.

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
        ("betweenness_centrality", "u"),
        ("bfs", "s"),
        ("closeness_centrality", "u"),
        ("clustering", "u"),
        ("compact", "u"),
        ("components", "s"),
        ("components_vector", "u"),
        ("contract", "u"),
        ("core_number", "u"),
        ("create_graph", "u"),
        ("current_version_is_kept", "u"),
        ("current_version_is_the_graphs", "u"),
        ("dag_longest_path", "u"),
        ("degree", "s"),
        ("degree_vector", "u"),
        ("diff", "r"),
        ("dijkstra", "s"),
        ("distance", "r"),
        ("drop_graph", "u"),
        ("drop_vector", "u"),
        ("drop_version", "u"),
        ("eigen", "u"),
        ("eigenvector_centrality", "u"),
        ("evict", "r"),
        ("explain_map", "r"),
        ("gather", "u"),
        ("grant", "u"),
        ("harmonic_centrality", "u"),
        ("has_edge", "s"),
        ("hits", "u"),
        ("id_map", "s"),
        ("info", "s"),
        ("katz_centrality", "u"),
        ("label_propagation", "u"),
        ("label_propagation_majority", "u"),
        ("label_vector_is_forgotten", "u"),
        ("last_stats", "r"),
        ("leiden", "u"),
        ("leiden_refine", "u"),
        ("lift", "u"),
        ("local_moving", "u"),
        ("louvain", "u"),
        ("matrix_rows", "r"),
        ("minimum_spanning_forest", "r"),
        ("modularity", "r"),
        ("most_similar", "r"),
        ("neighbours", "s"),
        ("pagerank", "u"),
        ("prewarm", "r"),
        ("project", "u"),
        ("promote", "u"),
        ("reachable", "s"),
        ("reachable_set", "s"),
        ("refresh", "u"),
        ("revoke", "u"),
        ("s", "s"),
        ("shared_cache", "s"),
        ("shortest_path", "s"),
        ("similar_pairs", "r"),
        ("similarity", "r"),
        ("spmm", "u"),
        ("spmv", "u"),
        ("sssp_vector", "u"),
        ("steps", "s"),
        ("strong_components_vector", "u"),
        ("topological_order", "u"),
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
        ("walk", "s"),
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
        "warren.minimum_spanning_forest('pp', NULL::bigint)",
        "warren.modularity('pp', 'c')",
        "warren.most_similar('pp', 1::bigint)",
        "warren.neighbours('pp', 1::bigint)",
        "warren.prewarm('pp')",
        "warren.reachable('pp', 1::bigint, 2::bigint)",
        "warren.reachable_set('pp', ARRAY[1::bigint])",
        "warren.s(NULL)",
        "warren.shared_cache()",
        "warren.shortest_path('pp', 1::bigint, 2::bigint)",
        "warren.similar_pairs('pp', NULL::bigint, nodes := 'c')",
        "warren.similarity('pp', 'SELECT 1, 3', NULL::bigint)",
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

    /// A query over each `PARALLEL SAFE` graph read (DESIGN.md §14 item
    /// 33), on graph `pp`, returning its rows as one text.
    const WORKER_READS: &[&str] = &[
        "SELECT string_agg(n::text, ',' ORDER BY n) FROM warren.neighbours('pp', 1::bigint) n",
        "SELECT warren.degree('pp', 1::bigint, 'both')::text",
        "SELECT warren.has_edge('pp', 1::bigint, 2::bigint)::text",
        "SELECT string_agg(m::text, ',' ORDER BY m.dense_id) FROM warren.id_map('pp', NULL::bigint) m",
        "SELECT warren.info('pp')::text",
        "SELECT warren.reachable('pp', 1::bigint, 4::bigint)::text",
        "SELECT warren.shortest_path('pp', 1::bigint, 4::bigint)::text",
        "SELECT string_agg(d::text, ',') FROM warren.dijkstra('pp', 1::bigint, 4::bigint) d",
        "SELECT string_agg(b::text, ',' ORDER BY b) FROM warren.bfs('pp', ARRAY[1::bigint], 2) b",
        "SELECT string_agg(r::text, ',' ORDER BY r) FROM warren.reachable_set('pp', ARRAY[1::bigint]) r",
        "SELECT string_agg(c::text, ',' ORDER BY c) FROM warren.components('pp', NULL::bigint) c",
        "SELECT string_agg(w::text, ',' ORDER BY w)
         FROM warren.walk('pp', ARRAY[1::bigint], ARRAY[warren.s(NULL), warren.s(NULL)]) w",
    ];

    /// The lines of `EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) <query>`.
    fn analyzed(query: &str) -> Result<Vec<String>, Error> {
        Spi::connect(|client| {
            client
                .select(&format!("EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) {query}"), None, &[])?
                .map(|row| row.get::<String>(1).map(Option::unwrap_or_default))
                .collect::<Result<Vec<_>, _>>()
        })
    }

    /// The first column of `query`'s first row, as text. Read without a
    /// row limit: under one (`Spi::get_one`'s), Postgres runs the plan
    /// without workers.
    fn text_of(query: &str) -> Result<Option<String>, Error> {
        Spi::connect(|client| client.select(query, None, &[])?.first().get_one::<String>())
    }

    /// The parallel workers the plan of `query` launched.
    fn workers_launched(query: &str) -> Result<usize, Error> {
        Ok(analyzed(query)?
            .iter()
            .filter_map(|line| line.trim().strip_prefix("Workers Launched: "))
            .filter_map(|count| count.parse::<usize>().ok())
            .sum())
    }

    #[pg_test]
    fn every_safe_graph_read_runs_in_a_worker_and_returns_the_leaders_rows() -> Result<(), Error> {
        graph_with_a_vector()?;
        for query in WORKER_READS {
            let in_leader = text_of(query)?;
            assert!(in_leader.is_some(), "{query} returned nothing");
            Spi::run(&format!("SET {FORCE_PARALLEL} = on"))?;
            assert_eq!(workers_launched(query)?, 1, "{query} did not run in a worker");
            let in_worker = text_of(query)?;
            Spi::run(&format!("RESET {FORCE_PARALLEL}"))?;
            assert_eq!(in_worker, in_leader, "{query}");
        }
        Ok(())
    }

    #[pg_test]
    fn a_parallel_scan_calls_safe_reads_per_row_in_its_workers() -> Result<(), Error> {
        graph_with_a_vector()?;
        Spi::run(
            "CREATE TABLE pairs AS SELECT g FROM generate_series(1, 100000) g; ANALYZE pairs",
        )?;
        let query = "SELECT count(*)::text || ' ' || sum(warren.degree('pp', g % 5 + 1))::text
                     FROM pairs WHERE warren.has_edge('pp', g % 5 + 1, (g + 1) % 5 + 1)";
        let serial = text_of(query)?;
        Spi::run(
            "SET parallel_setup_cost = 0; SET parallel_tuple_cost = 0;
             SET min_parallel_table_scan_size = 0; SET max_parallel_workers_per_gather = 2",
        )?;
        assert!(workers_launched(query)? >= 1, "{:?}", analyzed(query)?);
        let parallel = text_of(query)?;
        Spi::run(
            "RESET parallel_setup_cost; RESET parallel_tuple_cost;
             RESET min_parallel_table_scan_size; RESET max_parallel_workers_per_gather",
        )?;
        assert_eq!(parallel, serial);
        assert_eq!(serial.as_deref(), Some("100000 100000"));
        Ok(())
    }

    #[pg_test]
    fn a_traversal_in_a_worker_records_no_last_stats() -> Result<(), Error> {
        graph_with_a_vector()?;
        // A function that claims to be safe, so that it runs in the worker
        // and reads the worker's own last_stats() after a traversal there.
        Spi::run(
            "CREATE FUNCTION stats_in_a_worker() RETURNS text LANGUAGE plpgsql PARALLEL SAFE AS $$
             BEGIN
                 PERFORM count(*) FROM warren.bfs('pp', ARRAY[1::bigint], 2);
                 RETURN coalesce(warren.last_stats() ->> 'function', 'nothing');
             END $$",
        )?;
        Spi::run("SELECT warren.reachable('pp', 1::bigint, 4::bigint)")?;
        Spi::run(&format!("SET {FORCE_PARALLEL} = on"))?;
        assert_eq!(workers_launched("SELECT stats_in_a_worker()")?, 1);
        let in_worker = text_of("SELECT stats_in_a_worker()")?;
        let bfs = text_of("SELECT count(*)::text FROM warren.bfs('pp', ARRAY[1::bigint], 2)")?;
        Spi::run(&format!("RESET {FORCE_PARALLEL}"))?;
        assert_eq!(in_worker.as_deref(), Some("nothing"));
        assert_eq!(bfs.as_deref(), Some("3"));
        let in_leader = Spi::get_one::<String>("SELECT warren.last_stats() ->> 'function'")?;
        assert_eq!(in_leader.as_deref(), Some("reachable"), "a worker's bfs reached the leader");
        Ok(())
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
