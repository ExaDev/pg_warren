//! `warren.prewarm` and `warren.evict`: loading a graph's version into this
//! backend's cache ahead of use (for pooled connections), and freeing it
//! (DESIGN.md §5.5).

use pgrx::prelude::*;

use super::open::{cached, evict_graph, resolve};
use super::CacheError;
use crate::access::entry;
use crate::catalog::{GraphDefinition, VersionId};
use crate::source::with_id_type;

/// `warren.prewarm`: loads version `version` of graph `name` (its current
/// version if NULL) into this backend's cache and returns its id. Owners
/// and readers may call it.
#[pg_extern(schema = "warren", volatile, parallel_restricted)]
fn prewarm(name: &str, version: default!(Option<i64>, "NULL")) -> i64 {
    entry(|_| prewarm_version(name, version))
}

fn prewarm_version(name: &str, version: Option<VersionId>) -> Result<VersionId, CacheError> {
    let resolved = resolve(name, version, "read")?;
    let graph_id = resolved.graph.graph_id;
    with_id_type!(resolved.graph.id_type, E => cached::<E>(graph_id, resolved.version).map(drop))?;
    Ok(resolved.version)
}

/// `warren.evict`: frees this backend's cached versions of graph `name`
/// and returns how many there were. It changes nothing but this backend's
/// memory, so it needs no privilege on the graph.
#[pg_extern(schema = "warren", volatile, parallel_restricted)]
fn evict(name: &str) -> i64 {
    entry(|_| evict_versions(name))
}

fn evict_versions(name: &str) -> Result<i64, CacheError> {
    let graph = GraphDefinition::find(name)?;
    Ok(i64::try_from(evict_graph(graph.graph_id)).unwrap_or(i64::MAX))
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

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
