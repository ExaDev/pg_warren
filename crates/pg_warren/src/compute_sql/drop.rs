//! `warren.drop_vector` (DESIGN.md §6.2, §7): a management function, so
//! only members of the graph's owner role may call it.

use pgrx::prelude::*;

use super::{entry, ComputeSqlError};
use crate::access::require_owner;
use crate::cache::resolve;
use crate::vector::{forget, SpiVectorStore, VectorStore};

/// `warren.drop_vector`: deletes vector `name` bound to `version` (the
/// current version if NULL).
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn drop_vector(graph: Option<&str>, name: Option<&str>, version: default!(Option<i64>, "NULL")) {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let name = name.ok_or(ComputeSqlError::NullArgument("name"))?;
        let resolved = resolve(graph, version, "drop vectors of")?;
        require_owner(resolved.graph.owner, graph, "drop vectors of")?;
        let mut store = SpiVectorStore;
        let bound = store.bound(resolved.version)?;
        let record = bound.iter().find(|record| record.name == name).ok_or_else(|| {
            ComputeSqlError::VectorNotFound {
                graph: graph.to_owned(),
                name: name.to_owned(),
                version: resolved.version,
            }
        })?;
        forget(record.vector);
        store.delete(resolved.version, name)?;
        Ok(())
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, refresh, ring, small, versions};

    #[pg_test]
    fn a_dropped_vector_is_gone_with_its_chunks() -> Result<(), Error> {
        let version = small("t")?;
        Spi::run("SELECT warren.vector_const('t', 'c', 1); SELECT warren.drop_vector('t', 'c')")?;
        let left = Spi::get_one::<i64>(
            "SELECT (SELECT count(*) FROM warren.vectors) + (SELECT count(*) FROM warren.vector_chunks)",
        )?;
        assert_eq!(left, Some(0));
        let again = failure("warren.drop_vector('t', 'c')")?;
        let expected = format!("42704: graph \"t\" has no vector \"c\" at version {version}");
        assert_eq!(again, Some(expected));
        Ok(())
    }

    #[pg_test]
    fn only_owners_drop_vectors_and_readers_write_them() -> Result<(), Error> {
        small("t")?;
        Spi::run("CREATE ROLE analyst; SELECT warren.grant('t', 'analyst'); SET ROLE analyst")?;
        Spi::run("SELECT warren.vector_const('t', 'c', 1)")?;
        let refused = failure("warren.drop_vector('t', 'c')")?;
        Spi::run("RESET ROLE")?;
        assert_eq!(
            refused.as_deref(),
            Some("42501: must be a member of role \"t_owner\" to drop vectors of graph \"t\"")
        );
        Ok(())
    }

    #[pg_test]
    fn a_version_with_vectors_outlives_retention_until_dropped() -> Result<(), Error> {
        ring("kept", 4, 1)?;
        let first = refresh("kept", true)?;
        Spi::run("SELECT warren.vector_const('kept', 'score', 1)")?;
        refresh("kept", true)?;
        let third = refresh("kept", true)?;
        assert_eq!(versions("kept")?, Some(format!("{first}:false {third}:false")));
        Spi::run(&format!("SELECT warren.drop_version('kept', {first})"))?;
        let left = Spi::get_one::<i64>(
            "SELECT (SELECT count(*) FROM warren.vectors) + (SELECT count(*) FROM warren.vector_chunks)",
        )?;
        assert_eq!(left, Some(0));
        Ok(())
    }
}
