//! In-database test fixtures: small graphs over ordinary tables, created
//! inside the test's transaction.

use pgrx::prelude::*;
use pgrx::spi::Error;

/// Graph `name`: a directed ring `1 → 2 → … → nodes → 1` over tables
/// `<name>_nodes (id bigint)` and `<name>_edges (src bigint, dst bigint)`,
/// keeping `retain_versions` promoted versions.
pub(crate) fn ring(name: &str, nodes: i64, retain_versions: i32) -> Result<(), Error> {
    Spi::run(&format!(
        "CREATE TABLE {name}_nodes (id bigint);
         CREATE TABLE {name}_edges (src bigint, dst bigint);
         INSERT INTO {name}_nodes SELECT generate_series(1, {nodes});
         INSERT INTO {name}_edges SELECT g, g % {nodes} + 1 FROM generate_series(1, {nodes}) g;
         SELECT warren.create_graph('{name}', 'SELECT id FROM {name}_nodes',
                                    'SELECT src, dst FROM {name}_edges',
                                    retain_versions := {retain_versions})"
    ))
}

/// Refreshes graph `name` and returns the new version.
pub(crate) fn refresh(name: &str, promote: bool) -> Result<i64, Error> {
    Spi::get_one::<i64>(&format!("SELECT warren.refresh('{name}', promote := {promote})"))?
        .ok_or(Error::InvalidPosition)
}

/// The versions of graph `name`, ascending, as `version:staged` text.
pub(crate) fn versions(name: &str) -> Result<Option<String>, Error> {
    Spi::get_one::<String>(&format!(
        "SELECT coalesce(string_agg(version_id || ':' || staged, ' ' ORDER BY seq), '')
         FROM warren.versions v JOIN warren.graphs g USING (graph_id) WHERE g.name = '{name}'"
    ))
}

/// The current version of graph `name`.
pub(crate) fn current(name: &str) -> Result<Option<i64>, Error> {
    Spi::get_one::<i64>(&format!("SELECT current_version FROM warren.graphs WHERE name = '{name}'"))
}
