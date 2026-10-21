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

    #[pg_test]
    fn katz_centrality_is_networkxs() -> Result<(), Error> {
        small("t")?;
        compiled("t", "katz_centrality", "SELECT warren.katz_centrality('t', 'k')")?;
        assert_eq!(
            values("t", "k", 6)?.as_deref(),
            Some("1:0.383727 2:0.366458 3:0.364731 4:0.556422 5:0.410107 6:0.328085")
        );
        let iterations = Spi::get_one::<i32>("SELECT warren.katz_centrality('t', 'k')")?;
        assert_eq!(iterations, Some(11), "as many iterations as NetworkX needs");
        Spi::run(
            "SELECT warren.katz_centrality('t', 'k', alpha := 0.2, beta := 0.5, normalized := false)",
        )?;
        assert_eq!(
            values("t", "k", 6)?.as_deref(),
            Some("1:0.781562 2:0.656312 3:0.631262 4:1.407812 5:0.833333 6:0.500000")
        );
        Spi::run("SELECT warren.katz_centrality('t', 'k', rels := ARRAY['a'], weighted := false)")?;
        assert_eq!(
            values("t", "k", 6)?.as_deref(),
            Some("1:0.425717 2:0.429587 3:0.429974 4:0.387015 5:0.387015 6:0.387015")
        );
        // A per-node beta (NetworkX's dict beta) is beta × seed.
        Spi::run(
            "SELECT warren.vector_from_query('t', 'ids', 'SELECT id, id FROM t_nodes', 'i64');
             SELECT warren.katz_centrality('t', 'k', beta := 2, seed := 'ids')",
        )?;
        assert_eq!(
            values("t", "k", 6)?.as_deref(),
            Some("1:0.138746 2:0.197946 3:0.295902 4:0.467106 5:0.575223 6:0.552214")
        );
        Ok(())
    }

    #[pg_test]
    fn hits_is_networkxs() -> Result<(), Error> {
        ring("r", 4, 2)?;
        refresh("r", true)?;
        compiled("r", "hits", "SELECT warren.hits('r', 'hub', 'authority')")?;
        let uniform = Some("1:0.250000 2:0.250000 3:0.250000 4:0.250000");
        assert_eq!(values("r", "hub", 6)?.as_deref(), uniform);
        assert_eq!(values("r", "authority", 6)?.as_deref(), uniform);

        small("t")?;
        let iterations = Spi::get_one::<i32>("SELECT warren.hits('t', 'hub', 'authority')")?;
        assert_eq!(iterations, Some(11), "as many iterations as NetworkX needs");
        assert_eq!(
            values("t", "hub", 6)?.as_deref(),
            Some("1:0.838516 2:0.000000 3:0.161484 4:0.000000 5:0.000000 6:0.000000")
        );
        assert_eq!(
            values("t", "authority", 6)?.as_deref(),
            Some("1:0.000000 2:0.161484 3:0.000000 4:0.838516 5:0.000000 6:0.000000")
        );
        // Not normalized: hubs and authorities are scaled to a largest value of 1.
        Spi::run("SELECT warren.hits('t', 'hub', 'authority', normalized := false)")?;
        assert_eq!(
            values("t", "hub", 6)?.as_deref(),
            Some("1:1.000000 2:0.000000 3:0.192582 4:0.000000 5:0.000000 6:0.000000")
        );
        assert_eq!(
            values("t", "authority", 6)?.as_deref(),
            Some("1:0.000000 2:0.192582 3:0.000000 4:1.000000 5:0.000000 6:0.000000")
        );
        Spi::run(
            "SELECT warren.hits('t', 'hub', 'authority', rels := ARRAY['b'], weighted := false)",
        )?;
        assert_eq!(
            values("t", "hub", 6)?.as_deref(),
            Some("1:0.500000 2:0.000000 3:0.500000 4:0.000000 5:0.000000 6:0.000000")
        );
        Ok(())
    }
}
