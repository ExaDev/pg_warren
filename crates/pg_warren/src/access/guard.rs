//! The entry-point guard every `warren.*` SQL function runs under
//! (DESIGN.md §7).
//!
//! Elevated statements run as the catalog owner, and the access checks
//! run as the caller; both must resolve names — operators and functions as
//! well as relations — only in trusted schemas, or a caller could put its
//! own `=` or `pg_has_role` ahead of `pg_catalog` and have it run with the
//! catalog owner's privileges or answer the check. So [`entry`] pins
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
pub(crate) const TRUSTED_SEARCH_PATH: &str = "pg_catalog, pg_temp";

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
    with_setting(c"search_path", path, f)
}

/// Runs `f` with the transaction read-only, then restores its previous
/// state: the caller's SQL that a `warren.*` function runs to read (a
/// source, message or value query) can then write nothing, not even
/// through a VOLATILE function it calls, which a read-only cursor alone
/// would allow. (The `transaction_read_only` GUC cannot be set inside a
/// function, so this sets the backend's flag; an ERROR unwinds through the
/// guard that restores it, as does a subtransaction abort.)
pub(crate) fn with_read_only<R>(f: impl FnOnce() -> R) -> R {
    /// Restores `XactReadOnly` when dropped, including while unwinding.
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: the main thread's transaction state flag, restored to
            // the value read below.
            unsafe { pg_sys::XactReadOnly = self.0 };
        }
    }
    // SAFETY: reads and sets the main thread's transaction state flag,
    // which the executor checks before every write.
    let _restore = unsafe {
        let restore = Restore(pg_sys::XactReadOnly);
        pg_sys::XactReadOnly = true;
        restore
    };
    f()
}

/// Runs `f` with GUC `name` set to `value` at a new nest level, then pops
/// it, restoring the previous value; if `f` raises an ERROR, the
/// (sub)transaction abort pops it.
fn with_setting<R>(name: &CStr, value: &str, f: impl FnOnce() -> R) -> R {
    // A NUL cannot come from SQL text; an empty value is the safe fallback
    // (for search_path, only the implicit pg_catalog).
    let value = CString::new(value).unwrap_or_default();
    // SAFETY: plain backend calls with NUL-terminated strings that outlive
    // them; set_config_option raises an ERROR rather than return failure
    // for a session-level SET of a valid GUC.
    let level = unsafe {
        let level = pg_sys::NewGUCNestLevel();
        pg_sys::set_config_option(
            name.as_ptr(),
            value.as_ptr(),
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
    // SAFETY: pops the nest level opened above, restoring the setting.
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
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
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
        ("backends", "warren.backends()"),
        ("betweenness_centrality", "warren.betweenness_centrality('victim', 'bc')"),
        ("bfs", "warren.bfs('victim', ARRAY[1::bigint], 2)"),
        ("cache", "warren.cache()"),
        ("closeness_centrality", "warren.closeness_centrality('victim', 'cc', k := 2)"),
        ("clustering", "warren.clustering('victim', 'cl')"),
        ("compact", "warren.compact('victim', 'locality')"),
        ("components", "warren.components('victim', NULL::bigint)"),
        ("components_vector", "warren.components_vector('victim', 'cc')"),
        ("contract", "warren.contract('victim', 'c', 'eves_q')"),
        ("core_number", "warren.core_number('victim', 'k')"),
        (
            "create_graph",
            "warren.create_graph('eves', 'SELECT 1::bigint', 'SELECT 1::bigint, 2::bigint')",
        ),
        (
            "create_graph_from_keys",
            "warren.create_graph_from_keys('eves_k', ARRAY['pg_catalog.pg_namespace'::regclass])",
        ),
        ("dag_longest_path", "warren.dag_longest_path('victim', 'lp', ARRAY['none'])"),
        ("degree", "warren.degree('victim', 1::bigint)"),
        ("degree_vector", "warren.degree_vector('victim', 'dv')"),
        ("diff", "warren.diff('victim', {v}, {v})"),
        ("dijkstra", "warren.dijkstra('victim', 1::bigint, 2::bigint)"),
        ("distance", "warren.distance('victim', {v}, {v}, 'edit')"),
        ("drop_graph", "warren.drop_graph('victim')"),
        ("drop_orphans", "warren.drop_orphans()"),
        ("drop_owned", "warren.drop_owned('eve_minion')"),
        ("drop_vector", "warren.drop_vector('victim', 'c')"),
        ("drop_version", "warren.drop_version('victim', {v})"),
        ("eigen", "warren.eigen('victim', 1)"),
        ("eigenvector_centrality", "warren.eigenvector_centrality('victim', 'ev')"),
        ("evict", "warren.evict('victim')"),
        ("explain_map", "warren.explain_map('victim', 'c + 1')"),
        ("gather", "warren.gather('victim', 'SELECT 1::bigint, 1', NULL::bigint, NULL::int[])"),
        ("grant", "warren.grant('victim', 'eve')"),
        ("harmonic_centrality", "warren.harmonic_centrality('victim', 'hc')"),
        ("has_edge", "warren.has_edge('victim', 1::bigint, 2::bigint)"),
        ("hits", "warren.hits('victim', 'h', 'a')"),
        ("id_map", "warren.id_map('victim', NULL::bigint)"),
        ("info", "warren.info('victim')"),
        ("katz_centrality", "warren.katz_centrality('victim', 'k')"),
        ("label_propagation", "warren.label_propagation('victim', 'lp')"),
        ("label_propagation_majority", "warren.label_propagation_majority('victim', 'lpm')"),
        ("last_stats", "warren.last_stats()"),
        ("leiden", "warren.leiden('victim', 'ld')"),
        ("leiden_refine", "warren.leiden_refine('victim', 'c', 'r')"),
        ("lift", "warren.lift('victim_q', 'c', 'victim', 'l')"),
        ("local_moving", "warren.local_moving('victim', 'lm')"),
        ("louvain", "warren.louvain('victim', 'lv')"),
        ("matrix_rows", "warren.matrix_rows('victim', 'c', NULL::bigint)"),
        ("minimum_spanning_forest", "warren.minimum_spanning_forest('victim', NULL::bigint)"),
        ("modularity", "warren.modularity('victim', 'c')"),
        ("most_similar", "warren.most_similar('victim', 1::bigint)"),
        ("neighbours", "warren.neighbours('victim', 1::bigint)"),
        ("on_drop_owned", "warren.on_drop_owned()"),
        ("orphans", "warren.orphans()"),
        ("pagerank", "warren.pagerank('victim', 'pr')"),
        ("prewarm", "warren.prewarm('victim')"),
        ("progress", "warren.progress()"),
        ("project", "warren.project('victim', 'c', 'victim_q', 'p')"),
        ("promote", "warren.promote('victim', {v})"),
        (
            "queries_from_keys",
            "warren.queries_from_keys(ARRAY['pg_catalog.pg_namespace'::regclass])",
        ),
        ("reachable", "warren.reachable('victim', 1::bigint, 2::bigint)"),
        ("reachable_set", "warren.reachable_set('victim', ARRAY[1::bigint])"),
        ("reassign_owned", "warren.reassign_owned('eve_minion', 'eve')"),
        ("refresh", "warren.refresh('victim')"),
        ("report_progress", "warren.report_progress('mine', 'victim', 'phase')"),
        ("revoke", "warren.revoke('victim', 'eve')"),
        ("s", "warren.s(NULL)"),
        ("set_owner", "warren.set_owner('victim', 'eve')"),
        ("set_vector_quota", "warren.set_vector_quota('victim', 1000000, 'eve')"),
        ("shared_cache", "warren.shared_cache()"),
        ("shortest_path", "warren.shortest_path('victim', 1::bigint, 2::bigint)"),
        ("similar_pairs", "warren.similar_pairs('victim', NULL::bigint)"),
        ("similarity", "warren.similarity('victim', 'SELECT 1, 2', NULL::bigint)"),
        ("spmm", "warren.spmm('victim', 'c', 'y')"),
        ("spmv", "warren.spmv('victim', 'c', 'y', mask := 'c')"),
        ("sssp_vector", "warren.sssp_vector('victim', 'sp', 1::bigint)"),
        ("steps", "warren.steps('[]')"),
        ("strong_components_vector", "warren.strong_components_vector('victim', 'scc')"),
        ("topological_order", "warren.topological_order('victim', 'to', ARRAY['none'])"),
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
        ("vector_usage", "warren.vector_usage('victim')"),
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

    /// Trigger functions cannot be called, so they have no entry in
    /// `CALLS`; they fire inside elevated catalog statements, so each pins
    /// its own `search_path`, and PUBLIC has no EXECUTE on them.
    #[pg_test]
    fn every_trigger_function_pins_the_search_path_and_is_not_public() -> Result<(), Error> {
        let unpinned = Spi::get_one::<String>(
            "SELECT string_agg(proname, ',' ORDER BY proname) FROM pg_proc
             WHERE pronamespace = 'warren'::regnamespace AND prorettype = 'trigger'::regtype
               AND proconfig IS DISTINCT FROM '{\"search_path=pg_catalog, pg_temp\"}'",
        )?;
        assert_eq!(unpinned, None);
        let executable = Spi::get_one::<String>(
            "SELECT string_agg(proname, ',' ORDER BY proname) FROM pg_proc
             WHERE pronamespace = 'warren'::regnamespace AND prorettype = 'trigger'::regtype
               AND has_function_privilege('public', oid, 'EXECUTE')",
        )?;
        assert_eq!(executable, None, "PUBLIC may execute a trigger function");
        Ok(())
    }

    #[pg_test]
    fn every_warren_function_ignores_the_callers_search_path() -> Result<(), Error> {
        let functions = Spi::get_one::<String>(
            "SELECT string_agg(DISTINCT proname, ',' ORDER BY proname) FROM pg_proc
             WHERE pronamespace = 'warren'::regnamespace AND prorettype <> 'trigger'::regtype",
        )?;
        let called: Vec<&str> = CALLS.iter().map(|(name, _)| *name).collect();
        assert_eq!(functions, Some(called.join(",")), "every warren.* function has a call");

        ring("victim", 3, 2)?;
        let older = refresh("victim", true)?;
        let version = refresh("victim", true)?;
        hostile_eve()?;
        // Label vector `c` and quotient `victim_q`, which eve reads, so
        // that contract, project and lift run all the way as eve; and
        // `eve_minion`, whose privileges eve has, so that drop_owned and
        // reassign_owned run all the way too (it owns nothing); and a
        // quota on readers' vectors, so that eve's writes are counted.
        Spi::run(
            "CREATE ROLE eve_minion; GRANT eve_minion TO eve;
             SELECT warren.grant('victim', 'eve');
             SELECT warren.set_vector_quota('victim', 1000000);
             SELECT warren.vector_const('victim', 'c', 1);
             SELECT warren.contract('victim', 'c', 'victim_q');
             SELECT warren.vector_const('victim_q', 'c', 1);
             SELECT warren.grant('victim_q', 'eve')",
        )?;
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
        let ran = Spi::get_one::<String>(
            "SELECT (SELECT string_agg(name, ' ' ORDER BY name) FROM warren.graphs WHERE name = 'eves_q')
                    || ' ' || (SELECT string_agg(name, ' ' ORDER BY name) FROM warren.vectors
                               WHERE name IN ('p', 'l'))",
        )?;
        assert_eq!(ran.as_deref(), Some("eves_q l p"), "contract, project and lift ran as eve");
        let intact = Spi::get_one::<bool>(
            "SELECT EXISTS (SELECT 1 FROM warren.graphs
                            WHERE name = 'victim' AND readers = '{eve}'::regrole[])",
        )?;
        assert_eq!(intact, Some(true));
        Ok(())
    }
}
