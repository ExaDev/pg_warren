//! Tests of who may replace or drop a persisted vector (DESIGN.md §7):
//! its creator, or a member of the graph's owner role.

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, small};

    /// Graph `t` with readers `reader_a` and `reader_b`; `reader_a` has
    /// written vector `score`.
    fn two_readers() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "CREATE ROLE reader_a; CREATE ROLE reader_b;
             SELECT warren.grant('t', 'reader_a'); SELECT warren.grant('t', 'reader_b');
             SET ROLE reader_a; SELECT warren.vector_const('t', 'score', 1); RESET ROLE",
        )
    }

    /// The error of `call` as `role`.
    fn as_role(role: &str, call: &str) -> Result<Option<String>, Error> {
        // `failure`'s temporary table, usable by every role.
        Spi::run(
            "CREATE TEMP TABLE IF NOT EXISTS failure (what text);
             GRANT ALL ON failure TO PUBLIC",
        )?;
        Spi::run(&format!("SET ROLE {role}"))?;
        let found = failure(call);
        Spi::run("RESET ROLE")?;
        found
    }

    const REFUSED: &str = "42501: vector \"score\" of graph \"t\" was written by role \
                           \"reader_a\"; only that role or members of role \"t_owner\" may \
                           replace or drop it";

    #[pg_test]
    fn a_reader_may_not_replace_or_drop_another_readers_vector() -> Result<(), Error> {
        two_readers()?;
        let calls = [
            "warren.vector_const('t', 'score', 2)",
            "warren.vector_degree('t', 'score')",
            "warren.vector_dense_id('t', 'score')",
            "warren.vector_from_query('t', 'score', 'SELECT 1::bigint, 1')",
            "warren.spmv('t', 'score', 'score')",
            "warren.spmm('t', 'score', 'score')",
            "warren.vector_map('t', 'score', 'score + 1')",
            "warren.drop_vector('t', 'score')",
        ];
        for call in calls {
            assert_eq!(as_role("reader_b", call)?.as_deref(), Some(REFUSED), "{call}");
        }
        let created = Spi::get_one::<String>(
            "SELECT created_by::text || ':' || length FROM warren.vectors WHERE name = 'score'",
        )?;
        assert_eq!(created.as_deref(), Some("reader_a:6"), "the vector is untouched");
        Ok(())
    }

    #[pg_test]
    fn the_creator_and_owners_may_replace_and_drop_it() -> Result<(), Error> {
        two_readers()?;
        assert_eq!(as_role("reader_a", "warren.vector_const('t', 'score', 2)")?, None);
        assert_eq!(as_role("reader_b", "warren.vector_const('t', 'mine', 2)")?, None);
        assert_eq!(as_role("reader_b", "warren.drop_vector('t', 'mine')")?, None);
        assert_eq!(as_role("t_owner", "warren.vector_map('t', 'score', 'score * 2')")?, None);
        let created = Spi::get_one::<String>(
            "SELECT created_by::text FROM warren.vectors WHERE name = 'score'",
        )?;
        assert_eq!(created.as_deref(), Some("t_owner"), "a replacement is the replacer's");
        assert_eq!(
            as_role("reader_a", "warren.drop_vector('t', 'score')")?.as_deref(),
            Some(
                REFUSED
                    .replace("reader_a", "t_owner")
                    .replace("role \"t_owner\"; only", "role \"t_owner\"; only")
                    .as_str()
            ),
        );
        assert_eq!(as_role("t_owner", "warren.drop_vector('t', 'score')")?, None);
        Ok(())
    }

    #[pg_test]
    fn a_vector_whose_creator_was_dropped_is_the_owners() -> Result<(), Error> {
        two_readers()?;
        Spi::run("DROP ROLE reader_a")?;
        let refused = as_role("reader_b", "warren.drop_vector('t', 'score')")?.unwrap_or_default();
        assert!(refused.starts_with("42501: vector \"score\" of graph \"t\" was written by a role that no longer exists"), "{refused}");
        assert_eq!(as_role("t_owner", "warren.drop_vector('t', 'score')")?, None);
        Ok(())
    }
}
