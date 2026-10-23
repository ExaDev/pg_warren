//! Read-only statements: single-row SPI reads that never assign a
//! transaction id (DESIGN.md §6.9, §9.3).
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
//! [`read_one_latest`] reads the latest committed state (and the
//! transaction's own earlier writes) instead of the statement's snapshot,
//! for what must not depend on how old the caller's snapshot is. Depends on no other feature.

use std::ffi::{c_char, CString};
use std::ptr;

use pgrx::datum::DatumWithOid;
use pgrx::prelude::*;
use pgrx::spi::{SpiError, SpiErrorCodes, SpiResult};
use pgrx::{FromDatum, IntoDatum};

/// The first column of the first row `query` returns, read-only.
pub(crate) fn read_one<A: FromDatum + IntoDatum>(
    query: &str,
    args: &[DatumWithOid<'_>],
) -> SpiResult<Option<A>> {
    Spi::connect(|client| client.select(query, Some(1), args)?.first().get_one())
}

/// The first two columns of the first row `query` returns, read-only.
pub(crate) fn read_two<A: FromDatum + IntoDatum, B: FromDatum + IntoDatum>(
    query: &str,
    args: &[DatumWithOid<'_>],
) -> SpiResult<(Option<A>, Option<B>)> {
    Spi::connect(|client| client.select(query, Some(1), args)?.first().get_two())
}

/// Whether [`read_one_latest`] can read: not in parallel mode, where
/// Postgres refuses to take the latest snapshot.
pub(crate) fn latest_readable() -> bool {
    // SAFETY: reads the backend's parallel-mode flag, on the main thread.
    !unsafe { pg_sys::IsInParallelMode() }
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
    let mut types: Vec<pg_sys::Oid> = args.iter().map(DatumWithOid::oid).collect();
    let (mut values, nulls): (Vec<pg_sys::Datum>, Vec<c_char>) = args
        .iter()
        .map(|arg| match arg.datum() {
            Some(datum) => (datum.sans_lifetime(), b' ' as c_char),
            None => (pg_sys::Datum::from(0usize), b'n' as c_char),
        })
        .unzip();
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
