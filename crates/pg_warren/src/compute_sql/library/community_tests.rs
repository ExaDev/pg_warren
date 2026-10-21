//! In-database tests of the library's local and community structure
//! (DESIGN.md §6.7, planned library group 1): clustering and majority-vote
//! label propagation.

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::super::tests::{compiled, values};

    /// Graph `u`, undirected: a triangle 1–2–3 with a tail 3–4–5, node 6
    /// isolated, and `loops` added as self-loops.
    fn triangle_with_a_tail(loops: &[i64]) -> Result<(), Error> {
        let loops: String = loops.iter().map(|n| format!(", ({n}, {n})")).collect();
        Spi::run(&format!(
            "CREATE TABLE u_nodes (id bigint); INSERT INTO u_nodes SELECT generate_series(1, 6);
             CREATE TABLE u_edges (src bigint, dst bigint);
             INSERT INTO u_edges VALUES (1, 2), (2, 3), (3, 1), (3, 4), (4, 5){loops};
             SELECT warren.create_graph('u', 'SELECT id FROM u_nodes', 'SELECT src, dst FROM u_edges',
                                        directed := false);
             SELECT warren.refresh('u')"
        ))
    }

    #[pg_test]
    fn clustering_is_networkxs() -> Result<(), Error> {
        triangle_with_a_tail(&[])?;
        compiled("u", "clustering", "SELECT warren.clustering('u', 'c')")?;
        assert_eq!(
            values("u", "c", 6)?.as_deref(),
            Some("1:1.000000 2:1.000000 3:0.333333 4:0.000000 5:0.000000 6:0.000000")
        );
        Spi::run("SELECT warren.clustering('u', 'c', rels := ARRAY['nope'])")?;
        assert_eq!(
            values("u", "c", 6)?.as_deref(),
            Some("1:0.000000 2:0.000000 3:0.000000 4:0.000000 5:0.000000 6:0.000000")
        );
        Ok(())
    }

    /// NetworkX leaves self-loops out of the degree; `vector_degree` counts
    /// a node with a self-loop among its own neighbours, and no primitive
    /// tells which nodes have one (DESIGN.md §6.7), so such a node's
    /// coefficient is lower: 3's degree is 4, not 3.
    #[pg_test]
    fn clustering_counts_a_self_loop_in_the_degree() -> Result<(), Error> {
        triangle_with_a_tail(&[3])?;
        Spi::run("SELECT warren.clustering('u', 'c')")?;
        assert_eq!(
            values("u", "c", 6)?.as_deref(),
            Some("1:1.000000 2:1.000000 3:0.166667 4:0.000000 5:0.000000 6:0.000000")
        );
        Ok(())
    }
}
