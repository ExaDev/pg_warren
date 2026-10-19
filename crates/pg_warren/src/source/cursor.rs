//! Source queries over SPI: read-only cursors under the snapshot that
//! [`with_pinned_snapshot`] pins (DESIGN.md §5.4 step 1). Opening a query as
//! a read-only cursor also checks that it is a single SELECT that writes
//! nothing.

use std::ffi::{CStr, CString};
use std::ptr;

use pgrx::prelude::*;
use pgrx::spi::{SpiClient, SpiCursor, SpiHeapTupleData};
use pgrx::{PgList, PgTupleDesc};

use super::{ColumnType, SourceError};

/// Rows fetched from a cursor at a time.
const BATCH_ROWS: std::ffi::c_long = 10_000;

/// Runs `f` under one snapshot, taken after a command counter increment so
/// that it sees every earlier write of this transaction, including the
/// current statement's. Read-only cursors opened inside all use it.
pub(crate) fn with_pinned_snapshot<R>(f: impl FnOnce() -> R) -> R {
    // SAFETY: plain backend calls; the snapshot pushed here is popped below
    // or, if `f` raises an ERROR, by the (sub)transaction abort.
    unsafe {
        pg_sys::CommandCounterIncrement();
        pg_sys::PushActiveSnapshot(pg_sys::GetTransactionSnapshot());
    }
    let result = f();
    // SAFETY: pops the snapshot pushed above; `f` leaves the stack balanced.
    unsafe { pg_sys::PopActiveSnapshot() };
    result
}

/// The result column types of `query`, opened (not run) as a read-only
/// cursor as the current user.
pub(crate) fn source_columns<E>(name: &'static str, query: &str) -> Result<Vec<ColumnType>, E>
where
    E: From<SourceError> + From<pgrx::spi::Error>,
{
    Spi::connect(|client| open_read_only(client, name, query).map(|opened| opened.columns))
}

/// An open read-only cursor over a source query.
pub(crate) struct ReadOnlyCursor<'c> {
    /// The cursor.
    pub(crate) cursor: SpiCursor<'c>,
    /// Its result column types.
    pub(crate) columns: Vec<ColumnType>,
    /// The planner's estimate of its rows; a hint, often wrong.
    pub(crate) estimated_rows: u64,
}

/// Opens `query` as a read-only cursor under the active snapshot and
/// returns it with its result column types. Postgres raises an ERROR for a
/// query that cannot be a cursor (not a single SELECT, or not read-only).
pub(crate) fn open_read_only<'c, E>(
    client: &SpiClient<'c>,
    name: &'static str,
    query: &str,
) -> Result<ReadOnlyCursor<'c>, E>
where
    E: From<SourceError> + From<pgrx::spi::Error>,
{
    let text = CString::new(query).map_err(|_| SourceError::NulByte { query: name })?;
    // SAFETY: `text` is NUL-terminated and outlives the call. With no
    // arguments the argument arrays may be NULL. The cursor is read-only, so
    // it runs under the active snapshot. Postgres raises an ERROR rather
    // than return NULL.
    let portal = unsafe {
        pg_sys::SPI_cursor_open_with_args(
            ptr::null(),
            text.as_ptr(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
            true,
            0,
        )
    };
    // SAFETY: `portal` is the open portal just returned; its name is a
    // NUL-terminated string, and its tuple descriptor (set by PortalStart for
    // a SELECT) and planned statements live as long as the portal.
    let (cursor_name, columns, estimated_rows) = unsafe {
        (
            CStr::from_ptr((*portal).name).to_string_lossy().into_owned(),
            column_types((*portal).tupDesc),
            estimated_rows((*portal).stmts),
        )
    };
    Ok(ReadOnlyCursor { cursor: client.find_cursor(&cursor_name)?, columns, estimated_rows })
}

/// The planner's estimate of the rows a plan returns: the top plan node's
/// `plan_rows` (0 if there is none).
///
/// # Safety
///
/// `statements` is NULL or a valid list of `PlannedStmt`s.
unsafe fn estimated_rows(statements: *mut pg_sys::List) -> u64 {
    // SAFETY: the caller guarantees a valid list (or NULL, an empty list).
    let statements = unsafe { PgList::<pg_sys::PlannedStmt>::from_pg(statements) };
    statements
        .head()
        // SAFETY: list members are valid PlannedStmts; planTree may be NULL.
        .map(|statement| unsafe { (*statement).planTree })
        .filter(|plan| !plan.is_null())
        // SAFETY: a non-NULL planTree is a valid Plan node. The float to
        // integer cast saturates.
        .map_or(0, |plan| unsafe { (*plan).plan_rows } as u64)
}

/// The column types of a tuple descriptor (none if it is NULL).
///
/// # Safety
///
/// `descriptor` is NULL or a valid tuple descriptor.
unsafe fn column_types(descriptor: pg_sys::TupleDesc) -> Vec<ColumnType> {
    if descriptor.is_null() {
        return Vec::new();
    }
    // SAFETY: the caller guarantees a valid descriptor; it is not released.
    let descriptor = unsafe { PgTupleDesc::from_pg_unchecked(descriptor) };
    descriptor
        .iter()
        .filter(|attribute| !attribute.attisdropped)
        .map(|attribute| ColumnType {
            oid: attribute.atttypid,
            name: type_name(attribute.atttypid),
        })
        .collect()
}

/// A type's SQL name, as `format_type` prints it.
fn type_name(oid: pg_sys::Oid) -> String {
    // SAFETY: format_type_be returns a palloc'd NUL-terminated string for
    // any oid (or raises an ERROR).
    unsafe { CStr::from_ptr(pg_sys::format_type_be(oid)) }.to_string_lossy().into_owned()
}

/// Passes every remaining row of `cursor` to `each`, a batch at a time,
/// checking for interrupts between batches and freeing each batch after it.
pub(crate) fn for_each_row<E: From<pgrx::spi::Error>>(
    cursor: &mut SpiCursor<'_>,
    mut each: impl FnMut(&SpiHeapTupleData<'_>) -> Result<(), E>,
) -> Result<(), E> {
    loop {
        check_for_interrupts!();
        let batch = cursor.fetch(BATCH_ROWS)?;
        if batch.is_empty() {
            return Ok(());
        }
        for row in batch {
            each(&row)?;
        }
        // SAFETY: SPI_tuptable is the batch just consumed, which nothing
        // refers to any more.
        unsafe { pg_sys::SPI_freetuptable(pg_sys::SPI_tuptable) };
    }
}
