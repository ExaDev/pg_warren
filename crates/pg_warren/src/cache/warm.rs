//! `warren.prewarm` and `warren.evict`: loading a graph's version into this
//! backend's cache ahead of use (for pooled connections), and freeing it
//! (DESIGN.md §5.5).

use pgrx::prelude::*;

use super::open::{cached, evict_graph, prune_dropped, resolve};
use super::CacheError;
use crate::access::entry;
use crate::catalog::{GraphDefinition, VersionId};
use crate::source::with_id_type;
use crate::vector::{evict_transients, evict_vectors};

/// `warren.prewarm`: loads version `version` of graph `name` (its current
/// version if NULL) into this backend's cache and returns its id. Owners
/// and readers may call it.
#[pg_extern(schema = "warren", volatile, parallel_restricted)]
fn prewarm(name: &str, version: default!(Option<i64>, "NULL")) -> i64 {
    entry(|_| prewarm_version(name, version))
}

fn prewarm_version(name: &str, version: Option<VersionId>) -> Result<VersionId, CacheError> {
    let resolved = resolve(name, version, "read")?;
    with_id_type!(resolved.graph.id_type, E => cached::<E>(&resolved).map(drop))?;
    Ok(resolved.version)
}

/// `warren.evict`: frees this backend's cached versions, cached vectors
/// and transient vectors of graph `name` and returns how many there were.
/// It first frees, uncounted, what this backend holds of versions (of any
/// graph) that no longer exist. It changes nothing but this backend's
/// memory, so it needs no privilege on the graph.
#[pg_extern(schema = "warren", volatile, parallel_restricted)]
fn evict(name: &str) -> i64 {
    entry(|_| evict_versions(name))
}

fn evict_versions(name: &str) -> Result<i64, CacheError> {
    let graph = GraphDefinition::find(name)?;
    prune_dropped()?;
    let freed = evict_graph(graph.graph_id)
        + evict_vectors(graph.graph_id)
        + evict_transients(graph.graph_id);
    Ok(i64::try_from(freed).unwrap_or(i64::MAX))
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::cache::end_statement;
    use crate::fixture::{refresh, ring};

    #[pg_test]
    fn prewarm_loads_the_current_version_and_evict_frees_it() -> Result<(), Error> {
        ring("pw", 5, 3)?;
        let first = refresh("pw", true)?;
        let second = refresh("pw", true)?;
        assert_eq!(Spi::get_one::<i64>("SELECT warren.evict('pw')")?, Some(0));
        assert_eq!(Spi::get_one::<i64>("SELECT warren.prewarm('pw')")?, Some(second));
        let staged = Spi::get_one::<i64>(&format!("SELECT warren.prewarm('pw', {first})"))?;
        assert_eq!(staged, Some(first));
        assert_eq!(Spi::get_one::<i64>("SELECT warren.evict('pw')")?, Some(2));
        assert_eq!(Spi::get_one::<i64>("SELECT warren.evict('pw')")?, Some(0));
        Ok(())
    }

    #[pg_test]
    fn evict_frees_cached_vectors_too() -> Result<(), Error> {
        ring("pw", 5, 3)?;
        refresh("pw", true)?;
        Spi::run(
            "SELECT warren.vector_const('pw', 'a', 1); SELECT warren.vector_const('pw', 'b', 2)",
        )?;
        assert_eq!(Spi::get_one::<i64>("SELECT warren.evict('pw')")?, Some(3));
        Ok(())
    }

    #[pg_test]
    fn loading_the_current_version_drops_the_graphs_other_versions() -> Result<(), Error> {
        ring("pw", 5, 3)?;
        let first = refresh("pw", true)?;
        Spi::run(&format!("SELECT warren.prewarm('pw', {first})"))?;
        refresh("pw", true)?;
        Spi::run("SELECT warren.prewarm('pw')")?;
        assert_eq!(
            Spi::get_one::<i64>("SELECT warren.evict('pw')")?,
            Some(2),
            "not a version the statement has read"
        );
        Spi::run(&format!("SELECT warren.prewarm('pw', {first})"))?;
        end_statement();
        Spi::run("SELECT warren.prewarm('pw')")?;
        assert_eq!(Spi::get_one::<i64>("SELECT warren.evict('pw')")?, Some(1));
        Ok(())
    }

    #[pg_test]
    fn a_dropped_version_is_pruned_from_the_cache_on_the_next_load() -> Result<(), Error> {
        ring("pw", 5, 3)?;
        let first = refresh("pw", true)?;
        Spi::run("SELECT warren.prewarm('pw')")?;
        refresh("pw", true)?;
        Spi::run(&format!(
            "SELECT warren.drop_version('pw', {first}); SELECT warren.prewarm('pw')"
        ))?;
        assert_eq!(Spi::get_one::<i64>("SELECT warren.evict('pw')")?, Some(1));
        Ok(())
    }

    #[pg_test(error = "graph \"pw\" has no current version; refresh it, or pass a version")]
    fn a_graph_never_promoted_has_nothing_to_prewarm() -> Result<(), Error> {
        ring("pw", 5, 3)?;
        Spi::run("SELECT warren.prewarm('pw')")
    }

    #[pg_test(error = "graph \"pw\" has no version 999")]
    fn prewarming_an_unknown_version_fails() -> Result<(), Error> {
        ring("pw", 5, 3)?;
        Spi::run("SELECT warren.prewarm('pw', 999)")
    }

    #[pg_test(
        error = "must be a member of role \"pw_owner\" or of a reader role to read graph \"pw\""
    )]
    fn only_readers_may_prewarm() -> Result<(), Error> {
        ring("pw", 5, 3)?;
        refresh("pw", true)?;
        Spi::run("CREATE ROLE warmer; SET ROLE warmer; SELECT warren.prewarm('pw')")
    }
}
