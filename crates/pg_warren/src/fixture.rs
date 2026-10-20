//! In-database test fixtures: small graphs over ordinary tables, created
//! inside the test's transaction.

use pgrx::prelude::*;
use pgrx::spi::Error;

/// Graph `name`: a directed ring `1 → 2 → … → nodes → 1` over tables
/// `<name>_nodes (id bigint)` and `<name>_edges (src bigint, dst bigint)`,
/// keeping `retain_versions` promoted versions. The graph is owned by role
/// `<name>_owner`, created here, so that tests do not depend on the session
/// user; the session user stays a superuser and so a member of it.
pub(crate) fn ring(name: &str, nodes: i64, retain_versions: i32) -> Result<(), Error> {
    Spi::run(&format!(
        "CREATE ROLE {name}_owner;
         CREATE TABLE {name}_nodes (id bigint);
         CREATE TABLE {name}_edges (src bigint, dst bigint);
         INSERT INTO {name}_nodes SELECT generate_series(1, {nodes});
         INSERT INTO {name}_edges SELECT g, g % {nodes} + 1 FROM generate_series(1, {nodes}) g;
         GRANT SELECT ON {name}_nodes, {name}_edges TO {name}_owner;
         SET ROLE {name}_owner;
         SELECT warren.create_graph('{name}', 'SELECT id FROM {name}_nodes',
                                    'SELECT src, dst FROM {name}_edges',
                                    retain_versions := {retain_versions});
         RESET ROLE"
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

/// Graph `name` over `int8` ids, owned by role `<name>_owner`: nodes 1–6
/// and edges 1 → 2 (`a`, weight 1), 2 → 3 (`a`, 1), 3 → 4 (`b`, 1),
/// 1 → 4 (`b`, 5), 4 → 1 (`a`, 1); 5 → 5 (`b`, 2); 6 isolated. Weighted,
/// with relations and a reverse CSR, and refreshed.
pub(crate) fn small(name: &str) -> Result<i64, Error> {
    Spi::run(&format!(
        "CREATE ROLE {name}_owner;
         CREATE TABLE {name}_nodes (id bigint);
         CREATE TABLE {name}_edges (src bigint, dst bigint, rel text, w float8);
         INSERT INTO {name}_nodes SELECT generate_series(1, 6);
         INSERT INTO {name}_edges VALUES (1, 2, 'a', 1), (2, 3, 'a', 1), (3, 4, 'b', 1),
                                         (1, 4, 'b', 5), (4, 1, 'a', 1), (5, 5, 'b', 2);
         GRANT SELECT ON {name}_nodes, {name}_edges TO {name}_owner;
         SET ROLE {name}_owner;
         SELECT warren.create_graph('{name}', 'SELECT id FROM {name}_nodes',
                                    'SELECT src, dst, rel, w FROM {name}_edges',
                                    weighted := true, has_relations := true);
         RESET ROLE"
    ))?;
    refresh(name, true)
}

/// The rows of `query`, which returns one text column, joined by spaces.
pub(crate) fn rows(query: &str) -> Result<Option<String>, Error> {
    Spi::get_one::<String>(&format!("SELECT coalesce(string_agg(r, ' '), '') FROM ({query}) q(r)"))
}

/// Runs `PERFORM <expression>` and returns the SQLSTATE and message it
/// failed with, as `"<sqlstate>: <message>"`, or `None` if it succeeded.
pub(crate) fn failure(expression: &str) -> Result<Option<String>, Error> {
    Spi::run("CREATE TEMP TABLE IF NOT EXISTS failure (what text); TRUNCATE failure")?;
    Spi::run(&format!(
        "DO $do$ BEGIN PERFORM {expression};
         EXCEPTION WHEN OTHERS THEN
             INSERT INTO failure VALUES (SQLSTATE || ': ' || SQLERRM);
         END $do$"
    ))?;
    Spi::get_one::<String>("SELECT max(what) FROM failure")
}

/// Graphs `<name>_s`, which stores its merged CSR, and `<name>_u`, which
/// merges the relations' rows on the fly, over the same tables of `int8`
/// ids, owned by role `<name>_owner`: about 900 weighted edges in three
/// relations over 120 nodes, fractional weights, self-loops, many edges in
/// two relations and parallel edges within one. Each has two promoted
/// versions: after the first refresh, nodes divisible by 17 are deleted
/// (tombstoned) and nodes 121–125 added with edges.
pub(crate) fn merged_pair(name: &str) -> Result<(), Error> {
    merged_pair_with(name, true, true)
}

/// [`merged_pair`], weighted or not, with or without reverse CSRs.
pub(crate) fn merged_pair_with(
    name: &str,
    weighted: bool,
    build_reverse: bool,
) -> Result<(), Error> {
    let edge_query = match weighted {
        true => "SELECT src, dst, rel, w",
        false => "SELECT src, dst, rel",
    };
    Spi::run(&format!(
        "CREATE ROLE {name}_owner;
         CREATE TABLE {name}_nodes (id bigint);
         CREATE TABLE {name}_edges (src bigint, dst bigint, rel text, w float8);
         INSERT INTO {name}_nodes SELECT generate_series(1, 120);
         INSERT INTO {name}_edges
             SELECT s, d, 'r' || (s * 7 + d) % 3, ((s * d) % 10) / 10.0 + 0.1
             FROM generate_series(1, 120) s, generate_series(1, 120) d
             WHERE (s * 31 + d * 17) % 23 = 0 OR (s = d AND s % 9 = 0);
         INSERT INTO {name}_edges
             SELECT src, dst, 'r' || (substr(rel, 2)::int + 1) % 3, 0.7 FROM {name}_edges
             WHERE (src + dst) % 2 = 0;
         INSERT INTO {name}_edges SELECT src, dst, rel, 0.3 FROM {name}_edges
             WHERE (src + dst) % 5 = 0;
         GRANT SELECT ON {name}_nodes, {name}_edges TO {name}_owner;
         SET ROLE {name}_owner;
         SELECT warren.create_graph('{name}_s', 'SELECT id FROM {name}_nodes',
                                    '{edge_query} FROM {name}_edges',
                                    weighted := {weighted}, has_relations := true,
                                    build_reverse := {build_reverse}, store_merged := true);
         SELECT warren.create_graph('{name}_u', 'SELECT id FROM {name}_nodes',
                                    '{edge_query} FROM {name}_edges',
                                    weighted := {weighted}, has_relations := true,
                                    build_reverse := {build_reverse}, store_merged := false);
         RESET ROLE;"
    ))?;
    for graph in ["s", "u"] {
        refresh(&format!("{name}_{graph}"), true)?;
    }
    Spi::run(&format!(
        "DELETE FROM {name}_nodes WHERE id % 17 = 0;
         INSERT INTO {name}_nodes SELECT generate_series(121, 125);
         INSERT INTO {name}_edges SELECT n, n - 100, 'r' || n % 3, 1.5
             FROM generate_series(121, 125) n;
         INSERT INTO {name}_edges SELECT n - 101, n, 'r' || (n + 1) % 3, 0.25
             FROM generate_series(121, 125) n"
    ))?;
    for graph in ["s", "u"] {
        refresh(&format!("{name}_{graph}"), true)?;
    }
    Ok(())
}

/// The rows of `query` (one text column) on graph `<name>_s` and on
/// `<name>_u`, with `{g}` in `query` replaced by the graph's name, `{v}` by
/// its current version and `{p}` by its first: asserted identical, and
/// returned.
pub(crate) fn same_with_or_without_merged(name: &str, query: &str) -> Result<String, Error> {
    let on = |graph: &str| -> Result<String, Error> {
        let graph = format!("{name}_{graph}");
        let version = current(&graph)?.ok_or(Error::InvalidPosition)?;
        let first = Spi::get_one::<i64>(&format!(
            "SELECT min(version_id) FROM warren.versions v JOIN warren.graphs g USING (graph_id)
             WHERE g.name = '{graph}'"
        ))?
        .ok_or(Error::InvalidPosition)?;
        let query = query
            .replace("{g}", &graph)
            .replace("{v}", &version.to_string())
            .replace("{p}", &first.to_string());
        rows(&query)?.ok_or(Error::InvalidPosition)
    };
    let (stored, unmerged) = (on("s")?, on("u")?);
    assert_eq!(unmerged, stored, "{query}");
    assert!(!stored.is_empty(), "{query} returned no rows");
    Ok(stored)
}

/// The failure (`failure`) of `expression` on graph `<name>_s` and on
/// `<name>_u`, with `{g}` replaced by the graph's name: asserted identical,
/// and returned.
pub(crate) fn same_failure(name: &str, expression: &str) -> Result<Option<String>, Error> {
    let stored = failure(&expression.replace("{g}", &format!("{name}_s")))?;
    let unmerged = failure(&expression.replace("{g}", &format!("{name}_u")))?;
    assert_eq!(unmerged, stored, "{expression}");
    Ok(stored)
}

/// Runs `statements` on graph `<name>_s` and then on `<name>_u`, with `{g}`
/// replaced by the graph's name.
pub(crate) fn on_both(name: &str, statements: &str) -> Result<(), Error> {
    for graph in ["s", "u"] {
        Spi::run(&statements.replace("{g}", &format!("{name}_{graph}")))?;
    }
    Ok(())
}

/// Graphs `<prefix>_a` and `<prefix>_b`, rings of 3, each refreshed once;
/// returns their versions.
pub(crate) fn two_graphs(prefix: &str) -> Result<(i64, i64), Error> {
    ring(&format!("{prefix}_a"), 3, 2)?;
    ring(&format!("{prefix}_b"), 3, 2)?;
    Ok((refresh(&format!("{prefix}_a"), true)?, refresh(&format!("{prefix}_b"), true)?))
}

/// Sets graph `name`'s current version to `version` without the triggers'
/// check, as a restore can (DESIGN.md §5.1).
pub(crate) fn set_current_unchecked(name: &str, version: i64) -> Result<(), Error> {
    Spi::run(&format!(
        "SET session_replication_role = replica;
         UPDATE warren.graphs SET current_version = {version} WHERE name = '{name}';
         RESET session_replication_role"
    ))
}

/// The error a call raises when graph `name`'s current version `version`
/// is not one of its versions.
pub(crate) fn stray_current_failure(name: &str, version: i64) -> String {
    format!(
        "55000: graph \"{name}\" records current version {version}, which is not one of its \
         versions (a restored or edited catalog row); promote one of its versions"
    )
}
