//! `DISCARD ALL` (DESIGN.md §6.2, §9.3): what it resets of pg_warren's
//! session state. A session pooler runs it when a client leaves
//! (PgBouncer's `server_reset_query`), so the next client on the server
//! backend must not see what the last one left: the transient vectors
//! (of every role) and `last_stats()`. A `ProcessUtility` hook, installed
//! when the library loads, frees them once the statement has run; a
//! `DISCARD ALL` that fails (in a transaction block) frees nothing. The
//! caches of versions and persisted vectors are not session state: what
//! they hold is reloaded alike by any client, so they stay. Depends on
//! `cache`, `vector` and `traversal_sql`; nothing depends on it.

use std::ffi::c_char;

use pgrx::pg_sys;

use crate::cache::trim;
use crate::traversal_sql::forget_last_stats;
use crate::vector::retain_transients;

/// The hook installed before this one, which this one calls.
static mut PREVIOUS: pg_sys::ProcessUtility_hook_type = None;

/// Installs the hook (from `_PG_init`).
pub(crate) fn install() {
    // SAFETY: `_PG_init` runs once per process, on its main thread; the
    // hook is read and written only here and by Postgres, in that process.
    unsafe {
        PREVIOUS = pg_sys::ProcessUtility_hook;
        pg_sys::ProcessUtility_hook = Some(process_utility);
    }
}

/// Runs a utility statement as Postgres would, then, if it was `DISCARD
/// ALL`, frees pg_warren's session state.
#[pgrx::pg_guard]
#[allow(clippy::too_many_arguments)] // Postgres's signature for the hook.
unsafe extern "C-unwind" fn process_utility(
    statement: *mut pg_sys::PlannedStmt,
    query: *const c_char,
    read_only_tree: bool,
    context: pg_sys::ProcessUtilityContext::Type,
    params: pg_sys::ParamListInfo,
    environment: *mut pg_sys::QueryEnvironment,
    destination: *mut pg_sys::DestReceiver,
    completion: *mut pg_sys::QueryCompletion,
) {
    // SAFETY: Postgres passes a valid planned utility statement, read
    // before it runs; the previous hook is read by value, and the
    // arguments are passed on unchanged, as the hook's contract requires;
    // nothing with a destructor is live while the previous hook runs.
    unsafe {
        let utility = (*statement).utilityStmt;
        let discards_all = pgrx::is_a(utility, pg_sys::NodeTag::T_DiscardStmt)
            && (*utility.cast::<pg_sys::DiscardStmt>()).target == pg_sys::DiscardMode::DISCARD_ALL;
        match PREVIOUS {
            // A hook's ERROR (pg_stat_statements re-throws the statement's)
            // must unwind through pgrx's guard, as a `pg_sys` call's does.
            Some(previous) => pg_sys::ffi::pg_guard_ffi_boundary(|| {
                previous(
                    statement,
                    query,
                    read_only_tree,
                    context,
                    params,
                    environment,
                    destination,
                    completion,
                )
            }),
            None => pg_sys::standard_ProcessUtility(
                statement,
                query,
                read_only_tree,
                context,
                params,
                environment,
                destination,
                completion,
            ),
        }
        if discards_all {
            reset();
        }
    }
}

/// Frees pg_warren's session state, and records what the cache then
/// holds in the backend's activity slot (`warren.backends()`).
fn reset() {
    retain_transients(&[]);
    forget_last_stats();
    trim();
}
