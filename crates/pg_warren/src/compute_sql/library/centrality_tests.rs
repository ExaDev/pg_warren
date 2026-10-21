//! In-database tests of the library's centralities (DESIGN.md §6.7,
//! planned library group 1), against values NetworkX computes.

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::super::tests::{compiled, values};
    use crate::fixture::{refresh, ring, small};

    /// Graph `u`, undirected: a triangle 1–2–3 with a tail 3–4–5, and node
    /// 6 isolated.
    fn triangle_with_a_tail() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE u_nodes (id bigint); INSERT INTO u_nodes SELECT generate_series(1, 6);
             CREATE TABLE u_edges (src bigint, dst bigint);
             INSERT INTO u_edges VALUES (1, 2), (2, 3), (3, 1), (3, 4), (4, 5);
             SELECT warren.create_graph('u', 'SELECT id FROM u_nodes', 'SELECT src, dst FROM u_edges',
                                        directed := false);
             SELECT warren.refresh('u')",
        )
    }

    #[pg_test]
    fn eigenvector_centrality_is_networkxs() -> Result<(), Error> {
        ring("r", 4, 2)?;
        refresh("r", true)?;
        compiled("r", "eigenvector_centrality", "SELECT warren.eigenvector_centrality('r', 'ev')")?;
        assert_eq!(
            values("r", "ev", 6)?.as_deref(),
            Some("1:0.500000 2:0.500000 3:0.500000 4:0.500000")
        );
        let iterations = Spi::get_one::<i32>("SELECT warren.eigenvector_centrality('r', 'ev')")?;
        assert_eq!(iterations, Some(2), "the second iteration changes nothing");

        triangle_with_a_tail()?;
        compiled("u", "eigenvector_centrality", "SELECT warren.eigenvector_centrality('u', 'ev')")?;
        assert_eq!(
            values("u", "ev", 6)?.as_deref(),
            Some("1:0.497153 2:0.497153 3:0.603704 4:0.342487 5:0.154671 6:0.000000")
        );

        // Directed: centrality flows along edges (NetworkX's left
        // eigenvector); 5's self-loop counts once.
        small("t")?;
        Spi::run("SELECT warren.eigenvector_centrality('t', 'ev', weighted := false)")?;
        assert_eq!(
            values("t", "ev", 6)?.as_deref(),
            Some("1:0.525731 2:0.413304 3:0.324920 4:0.668740 5:0.000040 6:0.000000")
        );
        // Weighted, 5's self-loop (weight 2) competes with the cycle's
        // eigenvalue: NetworkX does not converge in 100 iterations either.
        let iterations = Spi::get_one::<i32>("SELECT warren.eigenvector_centrality('t', 'ev')")?;
        assert_eq!(iterations, Some(100));
        Ok(())
    }
}
