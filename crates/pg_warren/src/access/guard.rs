//! The entry-point guard every `warren.*` SQL function runs under
//! (DESIGN.md §7).
//!
//! Elevated statements run as the extension owner, and the access checks
//! run as the caller; both must resolve names — operators and functions as
//! well as relations — only in trusted schemas, or a caller could put its
//! own `=` or `pg_has_role` ahead of `pg_catalog` and have it run with the
//! extension owner's privileges or answer the check. So [`entry`] pins
//! `search_path` to `pg_catalog, pg_temp` for the whole function body, the
//! way `fmgr` applies `SECURITY DEFINER ... SET search_path`: a new GUC nest
//! level, `set_config_option(..., GUC_ACTION_SAVE)`, and `AtEOXact_GUC` on
//! return; a (sub)transaction abort restores it on error. SQL written for
//! SPI may therefore assume `search_path = pg_catalog, pg_temp`.

use std::ffi::{CStr, CString};
use std::ptr;

use pgrx::pg_sys;

use pgrx::{ereport, PgSqlErrorCode};

/// An error a `warren.*` function reports to its caller as a SQL error.
pub(crate) trait SqlError: std::fmt::Display {
    /// The SQLSTATE to report.
    fn sqlstate(&self) -> PgSqlErrorCode;
}

/// Unwraps `result`, or raises its error as a SQL `ERROR` (which does not
/// return: the transaction or subtransaction aborts).
fn or_raise<T, E: SqlError>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => {
            ereport!(ERROR, error.sqlstate(), error.to_string());
        }
    }
}

/// The `search_path` every `warren.*` function body runs under: the system
/// catalogs, then (explicitly last, so it can never shadow them) the
/// session's temporary schema.
const TRUSTED_SEARCH_PATH: &str = "pg_catalog, pg_temp";

/// What a `warren.*` function knows of its caller's session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Caller {
    search_path: String,
}

impl Caller {
    /// The caller's `search_path` before the guard pinned it: the one to
    /// run the caller's own SQL (a new graph's source queries) under.
    pub(crate) fn search_path(&self) -> &str {
        &self.search_path
    }
}

/// Runs the body of a `warren.*` SQL function under the trusted
/// `search_path`, and raises its error, if any, as a SQL error. Every
/// `#[pg_extern]` in this crate is written as `entry(|caller| ...)`.
pub(crate) fn entry<T, E: SqlError>(body: impl FnOnce(&Caller) -> Result<T, E>) -> T {
    let caller = Caller { search_path: current_search_path() };
    or_raise(with_search_path(TRUSTED_SEARCH_PATH, || body(&caller)))
}

/// Runs `f` with `search_path` set to `path`, then restores the previous
/// value. If `f` raises an ERROR, the (sub)transaction abort restores it.
pub(crate) fn with_search_path<R>(path: &str, f: impl FnOnce() -> R) -> R {
    // A NUL cannot come from SQL text; an empty path is the safe fallback
    // (only the implicit pg_catalog).
    let path = CString::new(path).unwrap_or_default();
    // SAFETY: plain backend calls with NUL-terminated strings that outlive
    // them; set_config_option raises an ERROR rather than return failure
    // for a session-level SET of a valid GUC.
    let level = unsafe {
        let level = pg_sys::NewGUCNestLevel();
        pg_sys::set_config_option(
            c"search_path".as_ptr(),
            path.as_ptr(),
            pg_sys::GucContext::PGC_USERSET,
            pg_sys::GucSource::PGC_S_SESSION,
            pg_sys::GucAction::GUC_ACTION_SAVE,
            true,
            0,
            false,
        );
        level
    };
    let result = f();
    // SAFETY: pops the nest level opened above, restoring search_path.
    unsafe { pg_sys::AtEOXact_GUC(true, level) };
    result
}

/// The session's current `search_path`, read from the GUC itself (not
/// through SQL, which the caller's own `search_path` would resolve).
fn current_search_path() -> String {
    // SAFETY: search_path always exists; the result is a palloc'd
    // NUL-terminated string.
    let value =
        unsafe { pg_sys::GetConfigOptionByName(c"search_path".as_ptr(), ptr::null_mut(), false) };
    if value.is_null() {
        return String::new();
    }
    // SAFETY: checked non-NULL above; NUL-terminated.
    unsafe { CStr::from_ptr(value) }.to_string_lossy().into_owned()
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{refresh, ring};

    /// Role `eve` (not a superuser) owns schema `eve`, where it defines `=`
    /// on text, bigint and oid and `||` on text, each counting its calls on
    /// sequence `public.probe_seq` (which a rolled-back call does not undo),
    /// and shadows
    /// `pg_roles` and `pg_has_role` with versions that say yes to anything.
    fn hostile_eve() -> Result<(), Error> {
        Spi::run(
            "CREATE ROLE eve;
             CREATE SCHEMA eve AUTHORIZATION eve;
             CREATE SEQUENCE public.probe_seq;
             GRANT USAGE ON public.probe_seq TO PUBLIC;
             SET ROLE eve;
             CREATE FUNCTION eve.teq(text, text) RETURNS boolean LANGUAGE plpgsql AS $f$
                 BEGIN
                     PERFORM pg_catalog.nextval('public.probe_seq');
                     RETURN $1 OPERATOR(pg_catalog.=) $2;
                 END $f$;
             CREATE OPERATOR eve.= (LEFTARG = text, RIGHTARG = text, FUNCTION = eve.teq);
             CREATE FUNCTION eve.beq(bigint, bigint) RETURNS boolean LANGUAGE plpgsql AS $f$
                 BEGIN
                     PERFORM pg_catalog.nextval('public.probe_seq');
                     RETURN $1 OPERATOR(pg_catalog.=) $2;
                 END $f$;
             CREATE OPERATOR eve.= (LEFTARG = bigint, RIGHTARG = bigint, FUNCTION = eve.beq);
             CREATE FUNCTION eve.oeq(oid, oid) RETURNS boolean LANGUAGE plpgsql AS $f$
                 BEGIN
                     PERFORM pg_catalog.nextval('public.probe_seq');
                     RETURN $1 OPERATOR(pg_catalog.=) $2;
                 END $f$;
             CREATE OPERATOR eve.= (LEFTARG = oid, RIGHTARG = oid, FUNCTION = eve.oeq);
             CREATE FUNCTION eve.tcat(text, text) RETURNS text LANGUAGE plpgsql AS $f$
                 BEGIN
                     PERFORM pg_catalog.nextval('public.probe_seq');
                     RETURN $1 OPERATOR(pg_catalog.||) $2;
                 END $f$;
             CREATE OPERATOR eve.|| (LEFTARG = text, RIGHTARG = text, FUNCTION = eve.tcat);
             CREATE FUNCTION eve.pg_has_role(oid, text) RETURNS boolean
                 LANGUAGE sql AS 'SELECT true';
             CREATE VIEW eve.pg_roles AS
                 SELECT oid, rolname, true AS rolsuper FROM pg_catalog.pg_roles;
             RESET ROLE",
        )
    }

    /// Runs `sql` as eve with eve's schema ahead of `pg_catalog`, ignoring
    /// any error it raises.
    fn as_eve(sql: &str) -> Result<(), Error> {
        Spi::run(&format!(
            "SET ROLE eve;
             SET search_path = eve, pg_catalog;
             DO $do$ BEGIN PERFORM {sql}; EXCEPTION WHEN OTHERS THEN NULL; END $do$;
             RESET search_path;
             RESET ROLE"
        ))
    }

    fn probes() -> Result<Option<i64>, Error> {
        Spi::get_one::<i64>(
            "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM public.probe_seq",
        )
    }

    #[pg_test]
    fn a_hostile_operator_on_the_callers_search_path_is_never_run() -> Result<(), Error> {
        ring("victim", 3, 2)?;
        refresh("victim", true)?;
        hostile_eve()?;
        Spi::run("SELECT warren.grant('victim', 'eve')")?;
        as_eve("warren.info('victim')")?;
        assert_eq!(probes()?, Some(0));
        Ok(())
    }

    #[pg_test]
    fn shadowed_role_catalogs_cannot_pass_the_owner_check() -> Result<(), Error> {
        ring("victim", 3, 2)?;
        hostile_eve()?;
        as_eve("warren.drop_graph('victim')")?;
        let kept = Spi::get_one::<bool>(
            "SELECT EXISTS (SELECT 1 FROM warren.graphs WHERE name = 'victim')",
        )?;
        assert_eq!(kept, Some(true));
        Ok(())
    }

    /// One call of every `warren.*` function, by name; `{v}` is a version
    /// of graph `victim`.
    const CALLS: &[(&str, &str)] = &[
        ("aggregate_neighbours", "warren.aggregate_neighbours('victim', 'c', 'a')"),
        ("bfs", "warren.bfs('victim', ARRAY[1::bigint], 2)"),
        ("compact", "warren.compact('victim')"),
        ("components", "warren.components('victim', NULL::bigint)"),
        ("components_vector", "warren.components_vector('victim', 'cc')"),
        (
            "create_graph",
            "warren.create_graph('eves', 'SELECT 1::bigint', 'SELECT 1::bigint, 2::bigint')",
        ),
        ("degree", "warren.degree('victim', 1::bigint)"),
        ("degree_vector", "warren.degree_vector('victim', 'dv')"),
        ("diff", "warren.diff('victim', {v}, {v})"),
        ("dijkstra", "warren.dijkstra('victim', 1::bigint, 2::bigint)"),
        ("drop_graph", "warren.drop_graph('victim')"),
        ("drop_vector", "warren.drop_vector('victim', 'c')"),
        ("drop_version", "warren.drop_version('victim', {v})"),
        ("evict", "warren.evict('victim')"),
        ("explain_map", "warren.explain_map('victim', 'c + 1')"),
        ("gather", "warren.gather('victim', 'SELECT 1::bigint, 1', NULL::bigint, NULL::int[])"),
        ("grant", "warren.grant('victim', 'eve')"),
        ("has_edge", "warren.has_edge('victim', 1::bigint, 2::bigint)"),
        ("id_map", "warren.id_map('victim', NULL::bigint)"),
        ("info", "warren.info('victim')"),
        ("label_propagation", "warren.label_propagation('victim', 'lp')"),
        ("last_stats", "warren.last_stats()"),
        ("matrix_rows", "warren.matrix_rows('victim', 'c', NULL::bigint)"),
        ("neighbours", "warren.neighbours('victim', 1::bigint)"),
        ("pagerank", "warren.pagerank('victim', 'pr')"),
        ("prewarm", "warren.prewarm('victim')"),
        ("promote", "warren.promote('victim', {v})"),
        ("reachable", "warren.reachable('victim', 1::bigint, 2::bigint)"),
        ("reachable_set", "warren.reachable_set('victim', ARRAY[1::bigint])"),
        ("refresh", "warren.refresh('victim')"),
        ("revoke", "warren.revoke('victim', 'eve')"),
        ("s", "warren.s(NULL)"),
        ("shortest_path", "warren.shortest_path('victim', 1::bigint, 2::bigint)"),
        ("spmm", "warren.spmm('victim', 'c', 'y')"),
        ("spmv", "warren.spmv('victim', 'c', 'y', mask := 'c')"),
        ("sssp_vector", "warren.sssp_vector('victim', 'sp', 1::bigint)"),
        ("steps", "warren.steps('[]')"),
        ("triangles", "warren.triangles('victim', 't')"),
        ("vector_const", "warren.vector_const('victim', 'c', 1)"),
        ("vector_degree", "warren.vector_degree('victim', 'd')"),
        ("vector_dense_id", "warren.vector_dense_id('victim', 'i')"),
        ("vector_from_query", "warren.vector_from_query('victim', 'q', 'SELECT 1::bigint, 1')"),
        ("vector_map", "warren.vector_map('victim', 'm', 'c + 1')"),
        ("vector_norm_diff", "warren.vector_norm_diff('victim', 'c', 'c')"),
        ("vector_reduce", "warren.vector_reduce('victim', 'c', 'sum')"),
        ("vector_stats", "warren.vector_stats('victim', 'c')"),
        ("vector_topk", "warren.vector_topk('victim', 'c', 3, NULL::bigint)"),
        ("vector_values", "warren.vector_values('victim', 'c', NULL::bigint)"),
        ("version", "warren.version()"),
        ("walk", "warren.walk('victim', ARRAY[1::bigint], ARRAY[warren.s(NULL)])"),
    ];

    #[pg_test]
    fn the_hostile_operators_run_where_the_search_path_is_not_pinned() -> Result<(), Error> {
        hostile_eve()?;
        as_eve("1::bigint = 1::bigint, 1::oid = 1::oid, 'a'::text = 'a', 'a'::text || 'b'")?;
        assert_eq!(probes()?, Some(4), "every probe operator logs");
        Ok(())
    }

    #[pg_test]
    fn every_warren_function_ignores_the_callers_search_path() -> Result<(), Error> {
        let functions = Spi::get_one::<String>(
            "SELECT string_agg(DISTINCT proname, ',' ORDER BY proname) FROM pg_proc
             WHERE pronamespace = 'warren'::regnamespace",
        )?;
        let called: Vec<&str> = CALLS.iter().map(|(name, _)| *name).collect();
        assert_eq!(functions, Some(called.join(",")), "every warren.* function has a call");

        ring("victim", 3, 2)?;
        let older = refresh("victim", true)?;
        let version = refresh("victim", true)?;
        hostile_eve()?;
        Spi::run("SELECT warren.grant('victim', 'eve')")?;
        for (_, call) in CALLS {
            // Leave only the older version cached, so that a call reading
            // the current one misses and prunes the cache (SPI over
            // warren.versions) inside the probe.
            Spi::run(&format!(
                "SELECT warren.evict('victim'); SELECT warren.prewarm('victim', {older})"
            ))?;
            as_eve(&call.replace("{v}", &version.to_string()))?;
        }
        assert_eq!(probes()?, Some(0), "the hostile operator ran");
        let intact = Spi::get_one::<bool>(
            "SELECT EXISTS (SELECT 1 FROM warren.graphs
                            WHERE name = 'victim' AND readers = '{eve}'::regrole[])",
        )?;
        assert_eq!(intact, Some(true));
        Ok(())
    }
}
