//! Saved plans: statements parsed and planned once per backend, on first
//! use, and kept for the backend's life (`SPI_keepplan`), so that a call
//! that runs one again skips its parse and plan (DESIGN.md §6.9).
//!
//! Postgres's plan cache keeps each one valid. A change to a table it
//! reads or writes (`ALTER EXTENSION ... UPDATE`, or `DROP EXTENSION` and
//! `CREATE EXTENSION`, whose tables are new ones), `DISCARD PLANS` and
//! `DISCARD ALL` invalidate it, and its next use parses and plans it again
//! from its text. Every statement names pg_warren's objects
//! schema-qualified, so planning it again under the guard's `search_path`
//! finds the same ones. A plan holds nothing of the client that ran it, so
//! it is not session state, and `DISCARD ALL` leaves it kept.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_long;
use std::rc::Rc;

use pgrx::datum::DatumWithOid;
use pgrx::spi::{OwnedPreparedStatement, SpiClient, SpiError, SpiResult, SpiTupleTable};
use pgrx::PgOid;

use super::{in_parallel_mode, select};

/// A saved plan's key: its statement, its arguments' types, and whether it
/// writes.
type Key = (&'static str, Vec<u32>, bool);

thread_local! {
    /// This backend's saved plans. Each is shared out of the map before it
    /// runs, so a statement that reaches pg_warren again finds the map
    /// unborrowed.
    static PLANS: RefCell<HashMap<Key, Rc<OwnedPreparedStatement>>> =
        RefCell::new(HashMap::new());
}

/// The saved plan of `query` for arguments of `args`' types, planned on
/// first use; writing if `mutating`.
fn plan(
    client: &SpiClient<'_>,
    query: &'static str,
    args: &[DatumWithOid<'_>],
    mutating: bool,
) -> SpiResult<Rc<OwnedPreparedStatement>> {
    let key = (query, args.iter().map(|arg| arg.oid().to_u32()).collect(), mutating);
    if let Some(plan) = PLANS.with_borrow(|plans| plans.get(&key).cloned()) {
        return Ok(plan);
    }
    let types: Vec<PgOid> = args.iter().map(|arg| PgOid::from(arg.oid())).collect();
    let prepared = match mutating {
        true => client.prepare_mut(query, &types)?,
        false => client.prepare(query, &types)?,
    };
    let plan = Rc::new(prepared.keep());
    PLANS.with_borrow_mut(|plans| plans.insert(key, Rc::clone(&plan)));
    Ok(plan)
}

/// Runs `query`, which writes or locks rows, through its saved plan, on a
/// [`super::writing`] connection.
pub(crate) fn update_saved<'c>(
    client: &mut SpiClient<'c>,
    query: &'static str,
    limit: Option<c_long>,
    args: &[DatumWithOid<'_>],
) -> SpiResult<SpiTupleTable<'c>> {
    let plan = plan(client, query, args, true)?;
    client.update(&*plan, limit, args)
}

/// Runs `query` through its saved plan without assigning a transaction id,
/// and passes its rows (at most `limit`, or all) to `f`. In parallel mode
/// it runs unprepared, as [`select`] runs it: a saved plan would run
/// non-read-only once the transaction has an id.
pub(crate) fn select_saved<R, E: From<SpiError>>(
    client: &SpiClient<'_>,
    query: &'static str,
    limit: Option<c_long>,
    args: &[DatumWithOid<'_>],
    f: impl FnOnce(SpiTupleTable<'_>) -> Result<R, E>,
) -> Result<R, E> {
    if in_parallel_mode() {
        return select(client, query, limit, args, f);
    }
    let plan = plan(client, query, args, false)?;
    f(client.select(&*plan, limit, args)?)
}
