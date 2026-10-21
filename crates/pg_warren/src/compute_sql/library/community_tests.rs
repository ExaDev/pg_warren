//! In-database tests of the library's local and community structure
//! (DESIGN.md §6.7, planned library group 1): clustering and majority-vote
//! label propagation.

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::super::tests::{compiled, values};
    use crate::fixture::small;

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

    /// Graph `s`, undirected: a star with centre 5 and leaves 1–4.
    fn star() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE s_nodes (id bigint); INSERT INTO s_nodes SELECT generate_series(1, 5);
             CREATE TABLE s_edges (src bigint, dst bigint);
             INSERT INTO s_edges VALUES (5, 1), (5, 2), (5, 3), (5, 4);
             SELECT warren.create_graph('s', 'SELECT id FROM s_nodes', 'SELECT src, dst FROM s_edges',
                                        directed := false);
             SELECT warren.refresh('s');
             SELECT warren.vector_from_query('s', 'ids', 'SELECT id, id FROM s_nodes')",
        )
    }

    #[pg_test]
    fn majority_label_propagation_takes_the_most_frequent_label_smallest_first() -> Result<(), Error>
    {
        star()?;
        // Step 1: each leaf sees its own label and 5 once each and keeps
        // the smaller, its own; the centre sees five labels once each and
        // takes 1. Step 2: each leaf sees its own label and 1. Step 3
        // changes nothing.
        let iterations = Spi::get_one::<i32>(
            "SELECT warren.label_propagation_majority('s', 'lp', seed := 'ids', max_iter := 1)",
        )?;
        assert_eq!(iterations, Some(1));
        assert_eq!(values("s", "lp", 0)?.as_deref(), Some("1:1 2:2 3:3 4:4 5:1"));
        compiled(
            "s",
            "label_propagation_majority",
            "SELECT warren.label_propagation_majority('s', 'lp', seed := 'ids')",
        )?;
        assert_eq!(values("s", "lp", 0)?.as_deref(), Some("1:1 2:1 3:1 4:1 5:1"));
        let iterations = Spi::get_one::<i32>(
            "SELECT warren.label_propagation_majority('s', 'lp', seed := 'ids')",
        )?;
        assert_eq!(iterations, Some(3));
        // Without a seed, each node starts with its dense id.
        Spi::run("SELECT warren.label_propagation_majority('s', 'lp')")?;
        assert_eq!(values("s", "lp", 0)?.as_deref(), Some("1:0 2:0 3:0 4:0 5:0"));
        Ok(())
    }

    #[pg_test]
    fn majority_label_propagation_ignores_direction_and_follows_rels() -> Result<(), Error> {
        small("t")?;
        // Undirected view: 1–2, 2–3, 3–4, 1–4; 5's self-loop counts its
        // own label twice; 6 is isolated. Dense ids are 0–5.
        Spi::run("SELECT warren.label_propagation_majority('t', 'lp')")?;
        assert_eq!(values("t", "lp", 0)?.as_deref(), Some("1:0 2:0 3:0 4:0 5:4 6:5"));
        // Relation b only: 3–4, 1–4 and 5's self-loop; 2 is alone.
        Spi::run("SELECT warren.label_propagation_majority('t', 'lp', rels := ARRAY['b'])")?;
        assert_eq!(values("t", "lp", 0)?.as_deref(), Some("1:0 2:1 3:0 4:0 5:4 6:5"));
        Ok(())
    }
}
