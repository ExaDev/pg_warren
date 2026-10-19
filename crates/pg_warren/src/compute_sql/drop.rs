//! `warren.drop_vector` (DESIGN.md §6.2, §7): a vector's creator, or a
//! member of the graph's owner role, may drop it.

use pgrx::prelude::*;

use super::{entry, ComputeSqlError};
use crate::access::require_creator_or_owner;
use crate::cache::resolve;
use crate::vector::{drop_transient, forget, SpiVectorStore, VectorError, VectorStore};

/// `warren.drop_vector`: deletes vector `name` bound to `version` (the
/// current version if NULL): the session's transient vector of that name
/// if there is one, else the persisted one. With `if_exists`, a missing
/// vector is not an error.
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn drop_vector(
    graph: Option<&str>,
    name: Option<&str>,
    version: default!(Option<i64>, "NULL"),
    if_exists: default!(Option<bool>, "false"),
) {
    entry(|_| {
        let if_exists = if_exists.ok_or(ComputeSqlError::NullArgument("if_exists"))?;
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let name = name.ok_or(ComputeSqlError::NullArgument("name"))?;
        let resolved = resolve(graph, version, "write vectors of")?;
        if drop_transient(resolved.graph.graph_id, resolved.version, name) {
            return Ok(());
        }
        let mut store = SpiVectorStore;
        let bound = store.bound(resolved.version)?;
        let Some(record) = bound.iter().find(|record| record.name == name) else {
            if if_exists {
                return Ok(());
            }
            return Err(ComputeSqlError::VectorNotFound {
                graph: graph.to_owned(),
                name: name.to_owned(),
                version: resolved.version,
            });
        };
        require_creator_or_owner(record.created_by, resolved.graph.owner, graph, name)?;
        // By id: the row checked above, never one of that name committed
        // since by another transaction.
        if !store.delete(record.vector)? {
            return Err(VectorError::WrittenConcurrently {
                name: name.to_owned(),
                version: resolved.version,
            }
            .into());
        }
        forget(record.vector);
        Ok(())
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, refresh, ring, small, versions};

    #[pg_test]
    fn if_exists_drops_a_missing_vector_quietly() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT warren.drop_vector('t', 'nope', if_exists := true)")?;
        let missing = failure("warren.drop_vector('t', 'nope')")?.unwrap_or_default();
        assert!(missing.starts_with("42704: graph \"t\" has no vector \"nope\""), "{missing}");
        Ok(())
    }

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
