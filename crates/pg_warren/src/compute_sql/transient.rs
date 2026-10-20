//! Tests of transient vectors (DESIGN.md §6.2): written with
//! `persist := false`, kept only in this backend, never in
//! `warren.vectors`.

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, refresh, ring, rows, small, versions};

    fn values(graph: &str, name: &str) -> Result<Option<String>, Error> {
        rows(&format!(
            "SELECT node_id || ':' || value FROM warren.vector_values('{graph}', '{name}', NULL::bigint)"
        ))
    }

    fn stored(name: &str) -> Result<Option<i64>, Error> {
        Spi::get_one::<i64>(&format!("SELECT count(*) FROM warren.vectors WHERE name = '{name}'"))
    }

    #[pg_test]
    fn every_vector_writer_can_write_a_transient_vector() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT warren.vector_const('t', 'x', 2, persist := false)")?;
        let calls = [
            "warren.vector_const('t', 'a', 1, persist := false)",
            "warren.vector_degree('t', 'b', persist := false)",
            "warren.vector_dense_id('t', 'c', persist := false)",
            "warren.vector_from_query('t', 'd', 'SELECT id, 1 FROM t_nodes', persist := false)",
            "warren.spmv('t', 'x', 'e', persist := false)",
            "warren.spmm('t', 'x', 'f', persist := false)",
            "warren.vector_map('t', 'g', 'x + 1', persist := false)",
        ];
        for call in calls {
            assert_eq!(failure(call)?, None, "{call}");
        }
        assert_eq!(Spi::get_one::<i64>("SELECT count(*) FROM warren.vectors")?, Some(0));
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT warren.vector_const('t', 'h', 1, persist := false) IS NULL"
            )?,
            Some(true),
            "a transient vector has no vector id"
        );
        assert_eq!(values("t", "g")?.as_deref(), Some("1:3 2:3 3:3 4:3 5:3 6:3"));
        assert_eq!(values("t", "b")?.as_deref(), Some("1:2 2:1 3:1 4:1 5:1 6:0"));
        let explained =
            Spi::get_one::<String>("SELECT warren.explain_map('t', 'g * 2') ->> 'path'")?;
        assert_eq!(explained.as_deref(), Some("compiled"));
        Ok(())
    }

    #[pg_test]
    fn writing_one_kind_drops_the_same_name_of_the_other() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT warren.vector_const('t', 'x', 1)")?;
        Spi::run("SELECT warren.vector_const('t', 'x', 2, persist := false)")?;
        assert_eq!(stored("x")?, Some(0), "the transient write dropped the persisted vector");
        assert_eq!(values("t", "x")?.as_deref(), Some("1:2 2:2 3:2 4:2 5:2 6:2"));
        Spi::run("SELECT warren.vector_const('t', 'x', 3)")?;
        assert_eq!(stored("x")?, Some(1));
        assert_eq!(values("t", "x")?.as_deref(), Some("1:3 2:3 3:3 4:3 5:3 6:3"));
        Spi::run("SELECT warren.vector_const('t', 'y', 4, persist := false)")?;
        Spi::run("SELECT warren.drop_vector('t', 'y')")?;
        let gone = failure("warren.vector_values('t', 'y', NULL::bigint)")?.unwrap_or_default();
        assert!(gone.starts_with("42704: graph \"t\" has no vector \"y\""), "{gone}");
        Ok(())
    }

    #[pg_test]
    fn a_transient_vector_survives_a_rollback_and_a_persisted_one_does_not() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "DO $$ BEGIN
                 PERFORM warren.vector_const('t', 'kept', 1, persist := false);
                 PERFORM warren.vector_const('t', 'undone', 1);
                 RAISE EXCEPTION 'roll back';
             EXCEPTION WHEN raise_exception THEN NULL;
             END $$",
        )?;
        assert_eq!(values("t", "kept")?.as_deref(), Some("1:1 2:1 3:1 4:1 5:1 6:1"));
        let undone = failure("warren.vector_values('t', 'undone', NULL::bigint)")?;
        assert!(undone.unwrap_or_default().starts_with("42704"));
        Ok(())
    }

    #[pg_test]
    fn a_transient_vector_does_not_pin_its_version_and_evict_frees_it() -> Result<(), Error> {
        ring("r", 5, 1)?;
        let first = refresh("r", true)?;
        Spi::run("SELECT warren.vector_const('r', 'scratch', 1, persist := false)")?;
        let second = refresh("r", true)?;
        assert_eq!(versions("r")?, Some(format!("{second}:false")), "retention dropped {first}");
        Spi::run("SELECT warren.vector_const('r', 'scratch', 1, persist := false)")?;
        // The loaded version and both transient vectors: retention's delete
        // of the first version has not committed, so its vector is kept
        // (test/sql/cache_dropped_transients.sql covers a committed drop).
        assert_eq!(Spi::get_one::<i64>("SELECT warren.evict('r')")?, Some(3));
        let gone = failure("warren.vector_values('r', 'scratch', NULL::bigint)")?;
        assert!(gone.unwrap_or_default().starts_with("42704"));
        Ok(())
    }

    #[pg_test]
    fn a_transient_write_may_not_drop_another_readers_persisted_vector() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "CREATE ROLE reader_a; CREATE ROLE reader_b;
             SELECT warren.grant('t', 'reader_a'); SELECT warren.grant('t', 'reader_b');
             CREATE TEMP TABLE failure (what text); GRANT ALL ON failure TO PUBLIC;
             SET ROLE reader_a; SELECT warren.vector_const('t', 'score', 1); RESET ROLE",
        )?;
        Spi::run("SET ROLE reader_b")?;
        let refused = failure("warren.vector_const('t', 'score', 2, persist := false)")?;
        Spi::run("RESET ROLE")?;
        assert!(refused.unwrap_or_default().starts_with("42501"));
        assert_eq!(stored("score")?, Some(1));
        Ok(())
    }

    #[pg_test]
    fn transient_vectors_are_per_role_within_a_session() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "CREATE ROLE reader_a; CREATE ROLE reader_b;
             SELECT warren.grant('t', 'reader_a'); SELECT warren.grant('t', 'reader_b');
             CREATE TEMP TABLE failure (what text); GRANT ALL ON failure TO PUBLIC;
             SET ROLE reader_a; SELECT warren.vector_const('t', 'x', 1, persist := false); RESET ROLE",
        )?;
        Spi::run("SET ROLE reader_b")?;
        // reader_b neither sees, shadows nor drops reader_a's transient x.
        let unseen = failure("warren.vector_values('t', 'x', NULL::bigint)")?;
        let undropped = failure("warren.drop_vector('t', 'x')")?;
        Spi::run("SELECT warren.vector_const('t', 'x', 2, persist := false)")?;
        let own = values("t", "x")?;
        let evicted = Spi::get_one::<i64>("SELECT warren.evict('t')")?;
        Spi::run("RESET ROLE; SET ROLE reader_a")?;
        let theirs = values("t", "x")?;
        Spi::run("RESET ROLE")?;
        assert!(unseen.unwrap_or_default().starts_with("42704"), "reader_b saw reader_a's x");
        assert!(
            undropped.unwrap_or_default().starts_with("42704"),
            "reader_b dropped reader_a's x"
        );
        assert_eq!(own.as_deref(), Some("1:2 2:2 3:2 4:2 5:2 6:2"));
        // The loaded version and reader_b's own transient x only.
        assert_eq!(evicted, Some(2));
        assert_eq!(theirs.as_deref(), Some("1:1 2:1 3:1 4:1 5:1 6:1"));
        Ok(())
    }

    #[pg_test(error = "persist must not be NULL")]
    fn persist_must_not_be_null() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT warren.vector_const('t', 'x', 1, persist := NULL)")
    }
}
