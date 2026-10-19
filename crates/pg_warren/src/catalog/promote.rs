//! `warren.promote`: makes a version of a graph current, then applies
//! retention (DESIGN.md §7, §5.4 step 6).

use pgrx::prelude::*;

use super::{
    promote_version, require_current_id_space, require_version, CatalogError, GraphDefinition,
    VersionId,
};
use crate::access::{entry, require_owner};

/// `warren.promote`: makes `version` the current version of graph `name`.
/// The caller must be a member of the owner role.
#[pg_extern(schema = "warren")]
fn promote(name: &str, version: i64) {
    entry(|_| promote_by_name(name, version));
}

fn promote_by_name(name: &str, version: VersionId) -> Result<(), CatalogError> {
    let mut graph = GraphDefinition::find(name)?;
    require_owner(graph.owner, &graph.name, "promote")?;
    graph.lock()?;
    require_version(&graph, version)?;
    require_current_id_space(&graph, version)?;
    promote_version(&graph, version)
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{current, refresh, ring, versions};

    #[pg_test]
    fn promote_makes_a_staged_version_current() -> Result<(), Error> {
        ring("p", 5, 2)?;
        let first = refresh("p", true)?;
        let staged = refresh("p", false)?;
        Spi::run(&format!("SELECT warren.promote('p', {staged})"))?;
        assert_eq!(current("p")?, Some(staged));
        assert_eq!(versions("p")?, Some(format!("{first}:false {staged}:false")));
        Ok(())
    }

    #[pg_test]
    fn promoting_deletes_promoted_versions_beyond_retain_versions() -> Result<(), Error> {
        ring("p", 5, 2)?;
        let first = refresh("p", true)?;
        let staged = refresh("p", false)?;
        let second = refresh("p", true)?;
        let third = refresh("p", true)?;
        assert_eq!(versions("p")?, Some(format!("{staged}:true {second}:false {third}:false")));
        let orphans = Spi::get_one::<i64>(&format!(
            "SELECT count(*) FROM warren.segments WHERE version_id = {first}"
        ))?;
        assert_eq!(orphans, Some(0));
        Ok(())
    }

    #[pg_test]
    fn promoting_an_older_version_keeps_it_as_current() -> Result<(), Error> {
        ring("p", 5, 1)?;
        let first = refresh("p", true)?;
        let staged = refresh("p", false)?;
        Spi::run(&format!("SELECT warren.promote('p', {first})"))?;
        assert_eq!(current("p")?, Some(first));
        Spi::run(&format!("SELECT warren.promote('p', {staged})"))?;
        assert_eq!(versions("p")?, Some(format!("{staged}:false")));
        Ok(())
    }

    #[pg_test]
    fn a_version_with_bound_vectors_is_retained() -> Result<(), Error> {
        ring("p", 5, 1)?;
        let first = refresh("p", true)?;
        Spi::run(&format!(
            "INSERT INTO warren.vectors (graph_id, version_id, name, dtype, length)
             SELECT graph_id, {first}, 'rank', 'f64', 5 FROM warren.graphs WHERE name = 'p'"
        ))?;
        let second = refresh("p", true)?;
        assert_eq!(versions("p")?, Some(format!("{first}:false {second}:false")));
        Ok(())
    }

    #[pg_test(error = "graph \"p\" has no version 999")]
    fn promoting_a_version_of_another_graph_fails() -> Result<(), Error> {
        ring("p", 5, 2)?;
        Spi::run("SELECT warren.promote('p', 999)")
    }

    #[pg_test(error = "must be a member of role \"p_owner\" to promote graph \"p\"")]
    fn only_the_owner_may_promote() -> Result<(), Error> {
        ring("p", 5, 2)?;
        let version = refresh("p", false)?;
        Spi::run(&format!(
            "CREATE ROLE promoter; SET ROLE promoter; SELECT warren.promote('p', {version})"
        ))
    }
}
