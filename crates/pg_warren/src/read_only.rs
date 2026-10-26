//! Read-only statements: SPI reads that never assign a transaction id
//! and run in parallel mode (DESIGN.md §6.9, §9.3).
//!
//! pgrx's `Spi::get_one*` and `get_two*` run their query through
//! `SpiClient::update`, which marks the transaction as writing by calling
//! `GetCurrentTransactionId()`: that assigns a transaction id, which
//! Postgres refuses in parallel mode ("cannot assign XIDs during a
//! parallel operation"). Every read on a path a query function takes goes
//! through here instead: `SpiClient::select` never calls `mark_mutable`, so
//! reading a graph never assigns a transaction id and works while a
//! parallel plan runs. It promises nothing about snapshots: as for any SPI
//! read, once the transaction has written, pgrx runs the select
//! non-read-only (a command counter increment and a fresh snapshot).
//! That increment is refused in parallel mode once the current command
//! has used its command id ("cannot start commands during a parallel
//! operation"), as in the leader (or a worker) of a parallel `CREATE
//! TABLE AS`, whose transaction has a transaction id: so in parallel mode
//! [`select`] runs the query read-only, under the active snapshot, as a
//! cursor. Every read on a query function's path goes through
//! [`select`] (or [`read_one`], [`read_two`]).
//! [`read_one_latest`] reads the latest committed state (and the
//! transaction's own earlier writes) instead of the statement's snapshot,
//! for what must not depend on how old the caller's snapshot is.
//!
//! Every write goes through [`writing`], the one way to open a writable
//! SPI connection. Its `update` assigns a transaction id, which a standby
//! (in recovery) refuses with an internal error ("cannot assign
//! TransactionIds during recovery"), so `writing` first refuses in a
//! read-only transaction, which every transaction on a standby is, with
//! the read-only SQLSTATE (DESIGN.md §5.1, §7). Depends on no other
//! feature.

use std::ffi::{c_char, c_long, CStr, CString};
use std::mem::ManuallyDrop;
use std::ptr;

use pgrx::datum::DatumWithOid;
use pgrx::pg_sys::panic::ErrorReport;
use pgrx::prelude::*;
use pgrx::spi::{SpiClient, SpiCursor, SpiError, SpiErrorCodes, SpiResult, SpiTupleTable};
use pgrx::{FromDatum, IntoDatum, PgLogLevel, PgSqlErrorCode};

/// The first column of the first row `query` returns, read-only.
pub(crate) fn read_one<A: FromDatum + IntoDatum>(
    query: &str,
    args: &[DatumWithOid<'_>],
) -> SpiResult<Option<A>> {
    Spi::connect(|client| select(client, query, Some(1), args, |rows| rows.first().get_one()))
}

/// The first two columns of the first row `query` returns, read-only.
pub(crate) fn read_two<A: FromDatum + IntoDatum, B: FromDatum + IntoDatum>(
    query: &str,
    args: &[DatumWithOid<'_>],
) -> SpiResult<(Option<A>, Option<B>)> {
    Spi::connect(|client| select(client, query, Some(1), args, |rows| rows.first().get_two()))
}

/// Whether the backend is in parallel mode: in a parallel worker, or in
/// the leader while a parallel plan runs.
pub(crate) fn in_parallel_mode() -> bool {
    // SAFETY: reads the backend's parallel-mode flag, on the main thread.
    unsafe { pg_sys::IsInParallelMode() }
}

/// Runs `query` without assigning a transaction id and passes its rows
/// (at most `limit`, or all) to `f`. Outside parallel mode it is
/// `SpiClient::select`; in parallel mode, a read-only [`open_cursor`].
/// If `f` raises an ERROR, the cursor is left for the abort to close, as
/// `source::cursor::Cursor` leaves its own (DESIGN.md §6.9).
pub(crate) fn select<R, E: From<SpiError>>(
    client: &SpiClient<'_>,
    query: &str,
    limit: Option<c_long>,
    args: &[DatumWithOid<'_>],
    f: impl FnOnce(SpiTupleTable<'_>) -> Result<R, E>,
) -> Result<R, E> {
    if !in_parallel_mode() {
        return f(client.select(query, limit, args)?);
    }
    let mut cursor = ManuallyDrop::new(open_cursor(client, query, args)?);
    let result = f(cursor.fetch(limit.unwrap_or(c_long::MAX))?);
    // Reached only without an ERROR: closes the cursor.
    drop(ManuallyDrop::into_inner(cursor));
    result
}

/// Opens `query` as a cursor without assigning a transaction id. Outside
/// parallel mode it is `SpiClient::try_open_cursor`; in parallel mode the
/// cursor is read-only, under the active snapshot, with no command
/// counter increment.
pub(crate) fn open_cursor<'c>(
    client: &SpiClient<'c>,
    query: &str,
    args: &[DatumWithOid<'_>],
) -> SpiResult<SpiCursor<'c>> {
    if !in_parallel_mode() {
        return client.try_open_cursor(query, args);
    }
    let text = CString::new(query).map_err(|_| SpiError::SpiError(SpiErrorCodes::Argument))?;
    let Arguments { mut types, mut values, nulls } = Arguments::from(args);
    let count =
        i32::try_from(types.len()).map_err(|_| SpiError::SpiError(SpiErrorCodes::Argument))?;
    // SAFETY: SPI is connected (`client`); `text` is NUL-terminated and the
    // argument arrays hold `count` entries, all outliving the call. Postgres
    // raises an ERROR rather than return NULL; the portal's name is a
    // NUL-terminated string that lives as long as the portal.
    let name = unsafe {
        let portal = pg_sys::SPI_cursor_open_with_args(
            ptr::null(),
            text.as_ptr(),
            count,
            types.as_mut_ptr(),
            values.as_mut_ptr(),
            nulls.as_ptr(),
            true,
            0,
        );
        CStr::from_ptr((*portal).name).to_string_lossy().into_owned()
    };
    client.find_cursor(&name)
}

/// SPI's argument arrays: types, values and nulls (`'n'` for NULL).
struct Arguments {
    types: Vec<pg_sys::Oid>,
    values: Vec<pg_sys::Datum>,
    nulls: Vec<c_char>,
}

impl From<&[DatumWithOid<'_>]> for Arguments {
    fn from(args: &[DatumWithOid<'_>]) -> Self {
        let types = args.iter().map(DatumWithOid::oid).collect();
        let (values, nulls) = args
            .iter()
            .map(|arg| match arg.datum() {
                Some(datum) => (datum.sans_lifetime(), b' ' as c_char),
                None => (pg_sys::Datum::from(0usize), b'n' as c_char),
            })
            .unzip();
        Self { types, values, nulls }
    }
}

/// Runs `f` on a writable SPI connection (`Spi::connect_mut`), after
/// refusing, with `read_only_sql_transaction` (25006), in a read-only
/// transaction: there the write would fail anyway, and on a standby
/// assigning the transaction id that `update` takes would fail first,
/// as an internal error.
#[allow(clippy::disallowed_methods)] // The one writable connection, checked first.
pub(crate) fn writing<R>(f: impl FnOnce(&mut SpiClient<'_>) -> R) -> R {
    // SAFETY: reads the main thread's transaction state flags.
    let (read_only, in_recovery) = unsafe { (pg_sys::XactReadOnly, pg_sys::RecoveryInProgress()) };
    if read_only {
        let refusal = ErrorReport::new(
            PgSqlErrorCode::ERRCODE_READ_ONLY_SQL_TRANSACTION,
            "cannot change pg_warren's catalog in a read-only transaction",
            "writing",
        );
        let refusal = if in_recovery {
            refusal.set_hint(
                "This server is a standby. Change graphs, versions and persisted vectors on the \
                 primary; persist := false keeps a vector in this session only.",
            )
        } else {
            refusal
        };
        refusal.report(PgLogLevel::ERROR);
    }
    Spi::connect_mut(f)
}

/// Whether [`read_one_latest`] can read: not in parallel mode, where
/// Postgres refuses to take the latest snapshot.
pub(crate) fn latest_readable() -> bool {
    !in_parallel_mode()
}

/// The first column of the first row `query` returns, read-only, under the
/// latest snapshot (what is committed now, not what the statement's
/// snapshot sees), whatever the isolation level and whether or not the
/// transaction has written. Like any snapshot, it also sees what this
/// transaction itself wrote in earlier commands, which a quota counts
/// (DESIGN.md §5.7). Only where [`latest_readable`].
pub(crate) fn read_one_latest<A: FromDatum + IntoDatum>(
    query: &str,
    args: &[DatumWithOid<'_>],
) -> SpiResult<Option<A>> {
    let query = CString::new(query).map_err(|_| SpiError::SpiError(SpiErrorCodes::Argument))?;
    let Arguments { mut types, mut values, nulls } = Arguments::from(args);
    let count =
        i32::try_from(types.len()).map_err(|_| SpiError::SpiError(SpiErrorCodes::Argument))?;
    Spi::connect(|_| {
        // SAFETY: SPI is connected for this closure; `types` holds `count`
        // argument types, and the plan lives until SPI disconnects.
        let plan = unsafe { pg_sys::SPI_prepare(query.as_ptr(), count, types.as_mut_ptr()) };
        if plan.is_null() {
            return Err(SpiError::SpiError(SpiErrorCodes::Argument));
        }
        // SAFETY: `values` and `nulls` hold one entry per argument type;
        // read-only execution under an explicit snapshot pushes (a copy
        // of) it as the active snapshot and pops it, and an ERROR's abort
        // unwinds the snapshot stack.
        let status = unsafe {
            pg_sys::SPI_execute_snapshot(
                plan,
                values.as_mut_ptr(),
                nulls.as_ptr(),
                pg_sys::GetLatestSnapshot(),
                ptr::null_mut(),
                true,
                false,
                1,
            )
        };
        Spi::check_status(status)?;
        // SAFETY: a successful SELECT left its rows in `SPI_tuptable`,
        // valid until SPI disconnects; the first row is read only if there
        // is one, and its datum converted (copied) before then, checked
        // against the column's type.
        unsafe {
            let table = pg_sys::SPI_tuptable;
            if table.is_null() || pg_sys::SPI_processed == 0 {
                return Ok(None);
            }
            let (row, descriptor) = (*(*table).vals, (*table).tupdesc);
            let mut null = false;
            let datum = pg_sys::SPI_getbinval(row, descriptor, 1, &mut null);
            Ok(A::try_from_datum(datum, null, pg_sys::SPI_gettypeid(descriptor, 1))?)
        }
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{refresh, ring};

    /// What `call` failed with, as `SQLSTATE: message`, in a read-only
    /// transaction (as every transaction on a standby is), or `None`. It
    /// runs in a subtransaction, whose abort (forced if `call` succeeds)
    /// makes the transaction read-write again.
    fn refused_read_only(call: &str) -> Result<Option<String>, Error> {
        Spi::run("CREATE TEMP TABLE IF NOT EXISTS refused (what text); TRUNCATE refused")?;
        Spi::run(&format!(
            "DO $do$ BEGIN
                 PERFORM pg_catalog.set_config('transaction_read_only', 'on', true);
                 PERFORM {call};
                 RAISE SQLSTATE 'P0001' USING MESSAGE = 'succeeded';
             EXCEPTION WHEN OTHERS THEN
                 IF SQLERRM <> 'succeeded' THEN
                     INSERT INTO refused VALUES (SQLSTATE || ': ' || SQLERRM);
                 END IF;
             END $do$"
        ))?;
        Spi::get_one::<String>("SELECT max(what) FROM refused")
    }

    #[pg_test]
    fn writers_are_refused_in_a_read_only_transaction_before_they_write() -> Result<(), Error> {
        ring("ro", 5, 2)?;
        refresh("ro", true)?;
        Spi::run("SELECT warren.vector_const('ro', 'c', 1)")?;
        let writers = [
            "warren.refresh('ro')",
            "warren.vector_const('ro', 'd', 1)",
            "warren.drop_vector('ro', 'c')",
            "warren.set_vector_quota('ro', 0)",
            "warren.create_graph('ro2', 'SELECT 1::bigint', 'SELECT 1::bigint, 1::bigint')",
        ];
        for writer in writers {
            assert_eq!(
                refused_read_only(writer)?.as_deref(),
                Some("25006: cannot change pg_warren's catalog in a read-only transaction"),
                "{writer}"
            );
        }
        Ok(())
    }

    #[pg_test]
    fn a_transient_vector_is_written_in_a_read_only_transaction() -> Result<(), Error> {
        ring("rt", 5, 2)?;
        refresh("rt", true)?;
        assert_eq!(refused_read_only("warren.vector_const('rt', 't', 1, persist := false)")?, None);
        Ok(())
    }
}
