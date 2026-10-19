//! Oracle tests: the compute functions against equivalent plain SQL over
//! the same tables, on a random graph (DESIGN.md §12).

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    /// Graph `r`: 2,000 nodes and 12,000 random edges (parallel ones
    /// included) in relations `p` and `q`, weights multiples of 0.5, so
    /// every sum below is exact in any order; and vector `x` of random
    /// integers.
    fn random_graph() -> Result<(), Error> {
        Spi::run(
            "SELECT setseed(0.42);
             CREATE ROLE r_owner;
             CREATE TABLE r_nodes AS SELECT generate_series(1, 2000)::bigint AS id;
             CREATE TABLE r_edges AS
                 SELECT (1 + floor(random() * 2000))::bigint AS src,
                        (1 + floor(random() * 2000))::bigint AS dst,
                        CASE WHEN random() < 0.5 THEN 'p' ELSE 'q' END AS rel,
                        floor(random() * 9) / 2 AS w
                 FROM generate_series(1, 12000);
             CREATE TABLE r_x AS
                 SELECT id, floor(random() * 201 - 100)::float8 AS value FROM r_nodes;
             GRANT SELECT ON r_nodes, r_edges TO r_owner;
             SET ROLE r_owner;
             SELECT warren.create_graph('r', 'SELECT id FROM r_nodes',
                                        'SELECT src, dst, rel, w FROM r_edges',
                                        weighted := true, has_relations := true);
             RESET ROLE;
             SELECT warren.refresh('r');
             SELECT warren.vector_from_query('r', 'x', 'SELECT id, value FROM r_x')",
        )
    }

    /// The rows where vector `name` and `expected (id, value)` disagree.
    fn disagreements(name: &str, expected: &str) -> Result<Option<i64>, Error> {
        Spi::get_one::<i64>(&format!(
            "WITH found AS (SELECT node_id AS id, value FROM warren.vector_values('r', '{name}', NULL::bigint)),
                  expected AS ({expected})
             SELECT count(*) FROM ((SELECT * FROM found EXCEPT SELECT * FROM expected)
                                   UNION ALL (SELECT * FROM expected EXCEPT SELECT * FROM found)) d"
        ))
    }

    /// `A[i][j]`: the summed weight of the edges `i → j` in `rels`.
    fn matrix(rels: &str) -> String {
        format!("SELECT src, dst, sum(w) AS w FROM r_edges WHERE rel IN ({rels}) GROUP BY src, dst")
    }

    #[pg_test]
    fn plus_times_is_a_join_and_a_sum() -> Result<(), Error> {
        random_graph()?;
        Spi::run("SELECT warren.spmv('r', 'x', 'y')")?;
        let expected = format!(
            "SELECT n.id, coalesce(sum(a.w * x.value), 0) FROM r_nodes n
             LEFT JOIN ({}) a ON a.src = n.id LEFT JOIN r_x x ON x.id = a.dst GROUP BY n.id",
            matrix("'p', 'q'")
        );
        assert_eq!(disagreements("y", &expected)?, Some(0));
        Ok(())
    }

    #[pg_test]
    fn transposed_plus_times_gathers_from_in_neighbours() -> Result<(), Error> {
        random_graph()?;
        Spi::run("SELECT warren.spmv('r', 'x', 'y', transpose := true, rels := ARRAY['q'])")?;
        let expected = format!(
            "SELECT n.id, coalesce(sum(a.w * x.value), 0) FROM r_nodes n
             LEFT JOIN ({}) a ON a.dst = n.id LEFT JOIN r_x x ON x.id = a.src GROUP BY n.id",
            matrix("'q'")
        );
        assert_eq!(disagreements("y", &expected)?, Some(0));
        Ok(())
    }

    #[pg_test]
    fn min_plus_is_a_join_and_a_min() -> Result<(), Error> {
        random_graph()?;
        Spi::run("SELECT warren.spmv('r', 'x', 'y', 'min_plus')")?;
        let expected = format!(
            "SELECT n.id, coalesce(min(a.w + x.value), 'Infinity') FROM r_nodes n
             LEFT JOIN ({}) a ON a.src = n.id LEFT JOIN r_x x ON x.id = a.dst GROUP BY n.id",
            matrix("'p', 'q'")
        );
        assert_eq!(disagreements("y", &expected)?, Some(0));
        Ok(())
    }

    #[pg_test]
    fn reductions_are_aggregates() -> Result<(), Error> {
        random_graph()?;
        let same = Spi::get_one::<bool>(
            "SELECT warren.vector_reduce('r', 'x', 'sum') = sum(value)
                AND warren.vector_reduce('r', 'x', 'max') = max(value)
                AND warren.vector_reduce('r', 'x', 'min') = min(value)
                AND warren.vector_reduce('r', 'x', 'l1') = sum(abs(value))
                AND warren.vector_reduce('r', 'x', 'nnz') = count(*) FILTER (WHERE value <> 0)
                AND abs(warren.vector_reduce('r', 'x', 'l2') - sqrt(sum(value ^ 2))) < 1e-9
             FROM r_x",
        )?;
        assert_eq!(same, Some(true));
        Ok(())
    }

    #[pg_test]
    fn vector_from_query_and_vector_values_round_trip() -> Result<(), Error> {
        random_graph()?;
        assert_eq!(disagreements("x", "SELECT id, value FROM r_x")?, Some(0));
        Ok(())
    }
}
