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

/// Bytes allocated in every Postgres memory context of the backend.
pub(crate) fn allocated() -> usize {
    // SAFETY: TopMemoryContext is valid for the life of the backend.
    unsafe { pg_sys::MemoryContextMemAllocated(pg_sys::TopMemoryContext, true) }
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

/// The rows of `warren.segments` this transaction has read so far, by
/// sequential and bitmap scans of the table and index scans of its indexes
/// (which count their heap fetches on the index): how a test sees a
/// version being loaded.
pub(crate) fn segment_reads() -> Result<i64, Error> {
    Spi::get_one::<i64>(
        "SELECT pg_stat_get_xact_tuples_returned('warren.segments'::regclass)
                + pg_stat_get_xact_tuples_fetched('warren.segments'::regclass)
                + sum(pg_stat_get_xact_tuples_fetched(indexrelid))::bigint
         FROM pg_index WHERE indrelid = 'warren.segments'::regclass",
    )?
    .ok_or(Error::InvalidPosition)
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

thread_local! {
    /// The WARNINGs `warnings` has captured.
    static WARNINGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// An `emit_log_hook` that keeps each WARNING's message.
#[pg_guard]
unsafe extern "C-unwind" fn keep_warnings(data: *mut pg_sys::ErrorData) {
    // SAFETY: Postgres passes the report it is emitting, valid for the call,
    // and its message, when set, is a NUL-terminated string.
    let message = unsafe {
        let data = &*data;
        (data.elevel == pg_sys::WARNING as i32 && !data.message.is_null())
            .then(|| std::ffi::CStr::from_ptr(data.message).to_string_lossy().into_owned())
    };
    WARNINGS.with(|kept| kept.borrow_mut().extend(message));
}

/// Puts back the `emit_log_hook` that `warnings` replaced, however the
/// statement ends.
struct Restore(pg_sys::emit_log_hook_type);

impl Drop for Restore {
    fn drop(&mut self) {
        // SAFETY: the hook is backend-global state that only the main
        // thread, which runs the test, touches.
        unsafe { pg_sys::emit_log_hook = self.0 };
    }
}

/// Runs `sql` and returns the messages of the WARNINGs it raised.
pub(crate) fn warnings(sql: &str) -> Result<Vec<String>, Error> {
    WARNINGS.with(|kept| kept.borrow_mut().clear());
    // SAFETY: as in `Restore::drop`; the guard puts the old hook back.
    let _restore = unsafe {
        let restore = Restore(pg_sys::emit_log_hook);
        pg_sys::emit_log_hook = Some(keep_warnings);
        restore
    };
    Spi::run(&format!("SET LOCAL log_min_messages = warning; {sql}"))?;
    Ok(WARNINGS.with(|kept| kept.take()))
}

/// [`failure`] of `expression` run as `role`.
pub(crate) fn failure_as(role: &str, expression: &str) -> Result<Option<String>, Error> {
    statement_failure_as(role, &format!("SELECT {expression}"))
}

/// Runs SQL `statement` (any statement, not only an expression) as `role`
/// (`none` for the session user) and returns the SQLSTATE and message it
/// failed with, as [`failure`] does, or `None` if it succeeded.
pub(crate) fn statement_failure_as(role: &str, statement: &str) -> Result<Option<String>, Error> {
    // `failure`'s temporary table, usable by every role.
    Spi::run(
        "CREATE TEMP TABLE IF NOT EXISTS failure (what text);
         GRANT ALL ON failure TO PUBLIC; TRUNCATE failure",
    )?;
    Spi::run(&format!("SET ROLE {role}"))?;
    let ran = Spi::run(&format!(
        "DO $do$ BEGIN EXECUTE $statement${statement}$statement$;
         EXCEPTION WHEN OTHERS THEN
             INSERT INTO failure VALUES (SQLSTATE || ': ' || SQLERRM);
         END $do$"
    ));
    Spi::run("RESET ROLE")?;
    ran?;
    Spi::get_one::<String>("SELECT max(what) FROM failure")
}

/// Graphs `dr`, owned by `dr_owner`, and `dk`, owned by `dk_owner` (rings
/// of 4, refreshed), where `dr_owner` is a reader of `dk`, wrote its
/// vector `mine` and has a quota on it, and `dk_owner` wrote `theirs`:
/// what a dropped role's cleanup must tell apart (DESIGN.md §5.7).
pub(crate) fn neighbouring_graphs() -> Result<(), Error> {
    ring("dr", 4, 2)?;
    ring("dk", 4, 2)?;
    refresh("dr", true)?;
    refresh("dk", true)?;
    Spi::run(
        "SELECT warren.grant('dk', 'dr_owner');
         SET ROLE dr_owner; SELECT warren.vector_const('dk', 'mine', 1); RESET ROLE;
         SET ROLE dk_owner; SELECT warren.vector_const('dk', 'theirs', 2);
         SELECT warren.set_vector_quota('dk', 4096, 'dr_owner'); RESET ROLE",
    )
}

/// Every row of `warren.vector_quotas`, as `graph:role:bytes`.
pub(crate) fn quotas() -> Result<Option<String>, Error> {
    Spi::get_one::<String>(
        "SELECT coalesce(string_agg(g.name || ':' || q.role::text || ':' || q.bytes, ' '
                                    ORDER BY g.name, q.role::text), '')
         FROM warren.vector_quotas q JOIN warren.graphs g USING (graph_id)",
    )
}

/// Every graph as `name:owner:readers`, then every persisted vector as
/// `graph.name:created_by`.
pub(crate) fn ownership() -> Result<Option<String>, Error> {
    Spi::get_one::<String>(
        "SELECT coalesce((SELECT string_agg(name || ':' || owner::text || ':' || readers::text,
                                            ' ' ORDER BY name) FROM warren.graphs), '')
                || ' / ' ||
                coalesce((SELECT string_agg(g.name || '.' || v.name || ':' || v.created_by::text,
                                            ' ' ORDER BY g.name, v.name)
                          FROM warren.vectors v JOIN warren.graphs g USING (graph_id)), '')",
    )
}

/// Graph `name`: a path `1 → 2 → … → nodes` over IRIs
/// (`https://example.org/resource/<n>`), refreshed.
pub(crate) fn iri_path(name: &str, nodes: i64) -> Result<(), Error> {
    Spi::run(&format!(
        "CREATE TABLE {name}_nodes AS
           SELECT n, 'https://example.org/resource/' || n AS id FROM generate_series(1, {nodes}) n;
         SELECT warren.create_graph('{name}', 'SELECT id FROM {name}_nodes',
                                    'SELECT a.id, b.id FROM {name}_nodes a JOIN {name}_nodes b ON b.n = a.n + 1');
         SELECT warren.refresh('{name}')"
    ))
}

/// The Postgres memory a set-returning `query` (a call in the select
/// list, so that its rows are made one at a time) holds once it has
/// returned its first row, past what the backend held before: what the
/// call keeps for the rows it has yet to return. Runs `query` to the end
/// once first, so that its version is cached and every later row checked.
pub(crate) fn held_after_first_row(query: &str) -> Result<usize, Error> {
    Spi::run(&format!("SELECT count(*) FROM (SELECT {query}) q"))?;
    Spi::connect(|client| {
        let before = allocated();
        let mut cursor = client.try_open_cursor(&format!("SELECT {query}"), &[])?;
        let first = cursor.fetch(1)?;
        let held = allocated().saturating_sub(before);
        assert_eq!(first.len(), 1, "the call returns a row");
        Ok(held)
    })
}

/// Graph `name`: a star over IRIs (`https://example.org/resource/<n>`),
/// hub 0 with an edge to each of leaves 1 to `leaves`, refreshed.
pub(crate) fn iri_star(name: &str, leaves: i64) -> Result<(), Error> {
    Spi::run(&format!(
        "CREATE TABLE {name}_nodes AS
           SELECT n, 'https://example.org/resource/' || n AS id FROM generate_series(0, {leaves}) n;
         SELECT warren.create_graph('{name}', 'SELECT id FROM {name}_nodes',
                                    'SELECT a.id, b.id FROM {name}_nodes a JOIN {name}_nodes b ON a.n = 0 AND b.n > 0');
         SELECT warren.refresh('{name}')"
    ))
}

/// Unweighted directed graphs over one edge list in three relations, its
/// pairs (self-loops aside) linked in one, two or three of them (about
/// 110, 185 and 25 pairs; DESIGN.md §5.3): `<name>_s`
/// stores the merged CSR and `<name>_u` does not; `<name>_each` is their
/// per-relation expansion, one weighted edge per pair weighing the number
/// of its relations; `<name>_once` is their deduplicated graph, one
/// unweighted edge per pair. Over 60 nodes, with self-loops, refreshed.
pub(crate) fn relation_expansions(name: &str) -> Result<(), Error> {
    Spi::run(&format!(
        "CREATE TABLE {name}_nodes AS SELECT generate_series(1, 60)::bigint AS id;
         CREATE TABLE {name}_edges AS
             SELECT s::bigint AS src, d::bigint AS dst, 'r' || r AS rel
             FROM generate_series(1, 60) s, generate_series(1, 60) d, generate_series(0, 2) r
             WHERE (s * 13 + d * 7) % 11 = 0 AND (s + d + r) % 4 <> 0 AND (s * d + r) % 5 <> 0
                OR s = d AND s % 7 = r;
         SELECT warren.create_graph('{name}_s', 'SELECT id FROM {name}_nodes',
                                    'SELECT src, dst, rel FROM {name}_edges', has_relations := true);
         SELECT warren.create_graph('{name}_u', 'SELECT id FROM {name}_nodes',
                                    'SELECT src, dst, rel FROM {name}_edges', has_relations := true,
                                    store_merged := false);
         SELECT warren.create_graph('{name}_each', 'SELECT id FROM {name}_nodes',
                                    'SELECT src, dst, count(*)::float8 FROM {name}_edges GROUP BY 1, 2',
                                    weighted := true);
         SELECT warren.create_graph('{name}_once', 'SELECT id FROM {name}_nodes',
                                    'SELECT DISTINCT src, dst FROM {name}_edges')"
    ))?;
    for graph in ["s", "u", "each", "once"] {
        refresh(&format!("{name}_{graph}"), true)?;
    }
    Ok(())
}

/// Weighted graph `name`: towns 1, 2 and 3, linked `1 → 2` by road (5)
/// and by rail (3), and `2 → 3` by road (1), refreshed. A cost reading
/// takes the rail (DESIGN.md §5.3): 1 to 2 costs 3, not 8.
pub(crate) fn road_and_rail(name: &str) -> Result<(), Error> {
    Spi::run(&format!(
        "CREATE TABLE {name}_towns (id bigint); INSERT INTO {name}_towns VALUES (1), (2), (3);
         CREATE TABLE {name}_links (src bigint, dst bigint, rel text, km float8);
         INSERT INTO {name}_links VALUES (1, 2, 'road', 5), (1, 2, 'rail', 3), (2, 3, 'road', 1);
         SELECT warren.create_graph('{name}', 'SELECT id FROM {name}_towns',
                                    'SELECT src, dst, rel, km FROM {name}_links',
                                    weighted := true, has_relations := true)"
    ))?;
    refresh(name, true).map(|_| ())
}

/// Weighted directed acyclic graphs over one edge list in three relations,
/// a pair's weight differing between its relations, its pairs linked in
/// one, two or three of them, as [`relation_expansions`]: `<name>_s` and
/// `<name>_u` (no merged CSR); `<name>_each`, one edge per pair weighing
/// `fold` (an SQL aggregate) of its relations' weights; `<name>_once`, one
/// edge per pair weighing their sum (DESIGN.md §5.3). Edges run from lower
/// ids to higher, over 60 nodes, refreshed.
pub(crate) fn weighted_relation_expansions(name: &str, fold: &str) -> Result<(), Error> {
    Spi::run(&format!(
        "CREATE TABLE {name}_nodes AS SELECT generate_series(1, 60)::bigint AS id;
         CREATE TABLE {name}_edges AS
             SELECT s::bigint AS src, d::bigint AS dst, 'r' || r AS rel,
                    (1 + (s * 3 + d + r * 2) % 5)::float8 AS w
             FROM generate_series(1, 60) s, generate_series(1, 60) d, generate_series(0, 2) r
             WHERE s < d AND (s * 13 + d * 7) % 11 = 0 AND (s + d + r) % 4 <> 0 AND (s * d + r) % 5 <> 0;
         SELECT warren.create_graph('{name}_s', 'SELECT id FROM {name}_nodes',
                                    'SELECT src, dst, rel, w FROM {name}_edges',
                                    weighted := true, has_relations := true);
         SELECT warren.create_graph('{name}_u', 'SELECT id FROM {name}_nodes',
                                    'SELECT src, dst, rel, w FROM {name}_edges',
                                    weighted := true, has_relations := true, store_merged := false);
         SELECT warren.create_graph('{name}_each', 'SELECT id FROM {name}_nodes',
                                    'SELECT src, dst, {fold}(w) FROM {name}_edges GROUP BY 1, 2',
                                    weighted := true);
         SELECT warren.create_graph('{name}_once', 'SELECT id FROM {name}_nodes',
                                    'SELECT src, dst, sum(w) FROM {name}_edges GROUP BY 1, 2',
                                    weighted := true)"
    ))?;
    for graph in ["s", "u", "each", "once"] {
        refresh(&format!("{name}_{graph}"), true)?;
    }
    Ok(())
}

/// Runs `statement` on the graphs of [`relation_expansions`] `name`, with
/// `{g}` replaced by the graph's name and `{per}` by `, per_relation :=
/// true` on `<name>_s` and `<name>_u` and by nothing on `<name>_each`, then
/// without it on all three and `<name>_once`, and asserts that vector `out`
/// (rounded to 9 significant digits) is the same per relation as on the
/// expansion, and by default as on the deduplicated graph; returns the two
/// readings' values, which must differ.
pub(crate) fn same_as_expanded(
    name: &str,
    statement: &str,
    out: &str,
) -> Result<(String, String), Error> {
    let on = |graph: &str, per: &str| vector_after(&format!("{name}_{graph}"), statement, per, out);
    let each = on("each", "")?;
    let once = on("once", "")?;
    for graph in ["s", "u"] {
        assert_eq!(on(graph, ", per_relation := true")?, each, "{graph}: {statement}");
        assert_eq!(on(graph, "")?, once, "{graph}: {statement}");
    }
    assert_ne!(each, once, "{statement}: the readings differ on this graph");
    Ok((each, once))
}

/// Runs `statement` on `graph` with `{g}` replaced by its name and `{per}`
/// by `per`, and returns vector `out`'s values, rounded to 9 significant
/// digits.
fn vector_after(graph: &str, statement: &str, per: &str, out: &str) -> Result<String, Error> {
    Spi::run(&statement.replace("{g}", graph).replace("{per}", per))?;
    rows(&format!(
        "SELECT node_id || ':' || CASE WHEN value = 0 THEN '0'
                                       ELSE to_char(value::float8, '9.99999999EEEE') END
         FROM warren.vector_values('{graph}', '{out}', NULL::bigint) ORDER BY node_id"
    ))?
    .ok_or(Error::InvalidPosition)
}

/// Runs `statement` as [`same_as_expanded`] does, for a cost reading
/// (DESIGN.md §5.3): asserts that vector `out` is the same on `<name>_s`
/// and `<name>_u`, with `per_relation := true` and without it, as on the
/// folded expansion `<name>_each`, and differs from `<name>_once`'s
/// summed weights.
pub(crate) fn cost_as_folded(name: &str, statement: &str, out: &str) -> Result<(), Error> {
    let on = |graph: &str, per: &str| vector_after(&format!("{name}_{graph}"), statement, per, out);
    let each = on("each", "")?;
    for graph in ["s", "u"] {
        for per in ["", ", per_relation := true", ", per_relation := false"] {
            assert_eq!(on(graph, per)?, each, "{graph}{per}: {statement}");
        }
    }
    assert_ne!(on("once", "")?, each, "{statement}: summing would differ on this graph");
    Ok(())
}
