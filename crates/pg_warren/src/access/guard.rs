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

    /// Role `eve` (not a superuser) owns schema `eve`, where it defines an
    /// `=` on text that logs who ran it to `public.probe_log`, and shadows
    /// `pg_roles` and `pg_has_role` with versions that say yes to anything.
    fn hostile_eve() -> Result<(), Error> {
        Spi::run(
            "CREATE ROLE eve;
             CREATE SCHEMA eve AUTHORIZATION eve;
             CREATE TABLE public.probe_log (who text);
             GRANT INSERT ON public.probe_log TO PUBLIC;
             SET ROLE eve;
             CREATE FUNCTION eve.teq(text, text) RETURNS boolean LANGUAGE plpgsql AS $f$
                 BEGIN
                     INSERT INTO public.probe_log VALUES (current_user::text);
                     RETURN $1 OPERATOR(pg_catalog.=) $2;
                 END $f$;
             CREATE OPERATOR eve.= (LEFTARG = text, RIGHTARG = text, FUNCTION = eve.teq);
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
        Spi::get_one::<i64>("SELECT count(*) FROM public.probe_log")
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
        ("compact", "warren.compact('victim')"),
        (
            "create_graph",
            "warren.create_graph('eves', 'SELECT 1::bigint', 'SELECT 1::bigint, 2::bigint')",
        ),
        ("diff", "warren.diff('victim', {v}, {v})"),
        ("drop_graph", "warren.drop_graph('victim')"),
        ("drop_version", "warren.drop_version('victim', {v})"),
        ("grant", "warren.grant('victim', 'eve')"),
        ("info", "warren.info('victim')"),
        ("promote", "warren.promote('victim', {v})"),
        ("refresh", "warren.refresh('victim')"),
        ("revoke", "warren.revoke('victim', 'eve')"),
        ("version", "warren.version()"),
    ];

    #[pg_test]
    fn every_warren_function_ignores_the_callers_search_path() -> Result<(), Error> {
        let functions = Spi::get_one::<String>(
            "SELECT string_agg(DISTINCT proname, ',' ORDER BY proname) FROM pg_proc
             WHERE pronamespace = 'warren'::regnamespace",
        )?;
        let called: Vec<&str> = CALLS.iter().map(|(name, _)| *name).collect();
        assert_eq!(functions, Some(called.join(",")), "every warren.* function has a call");

        ring("victim", 3, 2)?;
        let version = refresh("victim", true)?;
        hostile_eve()?;
        Spi::run("SELECT warren.grant('victim', 'eve')")?;
        for (_, call) in CALLS {
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
