//! Source queries over SPI: read-only cursors under the snapshot that
//! [`with_pinned_snapshot`] pins (DESIGN.md §5.4 step 1). Opening a query as
//! a read-only cursor also checks that it is a single SELECT that writes
//! nothing.

use std::ffi::{CStr, CString};
use std::ptr;

use pgrx::prelude::*;
use pgrx::spi::{SpiClient, SpiCursor, SpiTupleTable};
use pgrx::{PgList, PgMemoryContexts, PgTupleDesc};

use super::{ColumnType, IdDatum, SourceError};

/// Rows fetched from a cursor at a time.
const BATCH_ROWS: std::ffi::c_long = 10_000;

/// The bytes the rows of a batch may hold in their memory context (what
/// reading them detoasts) before it is reset, within the batch.
const RESET_BYTES: u64 = 64 << 20;

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

/// The result column types of `query`, from parse analysis alone: it is
/// prepared (parsed, analysed and rewritten) but neither planned nor
/// executed. It must be one statement that returns rows as a SELECT does
/// (a SELECT, VALUES or TABLE); whether it is read-only is checked when it
/// is opened.
pub(crate) fn describe<E>(name: &'static str, query: &str) -> Result<Vec<ColumnType>, E>
where
    E: From<SourceError>,
{
    let text = CString::new(query).map_err(|_| SourceError::NulByte { query: name })?;
    // SAFETY: inside an SPI connection; `text` is NUL-terminated and
    // outlives the call, which raises an ERROR on a bad query rather than
    // return NULL. The plan's sources and their result descriptors live
    // until SPI_freeplan, after the columns are copied out.
    unsafe {
        let plan = pg_sys::SPI_prepare(text.as_ptr(), 0, ptr::null_mut());
        let sources =
            PgList::<pg_sys::CachedPlanSource>::from_pg(pg_sys::SPI_plan_get_plan_sources(plan));
        let columns = match (sources.len(), sources.head()) {
            (1, Some(source)) if (*source).commandTag == pg_sys::CommandTag::CMDTAG_SELECT => {
                Some(column_types((*source).resultDesc))
            }
            _ => None,
        };
        pg_sys::SPI_freeplan(plan);
        Ok(columns.ok_or(SourceError::NotASelect { query: name })?)
    }
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
            nondeterministic: nondeterministic(attribute.attcollation),
        })
        .collect()
}

/// The name of `collation` if it is a nondeterministic collation.
fn nondeterministic(collation: pg_sys::Oid) -> Option<String> {
    if collation == pg_sys::Oid::INVALID {
        return None;
    }
    // SAFETY: a valid collation oid, from a tuple descriptor; both calls
    // read the syscache and raise an ERROR for an unknown oid rather than
    // return a bad value. get_collation_name returns NULL or a palloc'd
    // NUL-terminated string.
    unsafe {
        if pg_sys::get_collation_isdeterministic(collation) {
            return None;
        }
        let name = pg_sys::get_collation_name(collation);
        Some(match name.is_null() {
            true => collation.to_u32().to_string(),
            false => CStr::from_ptr(name).to_string_lossy().into_owned(),
        })
    }
}

/// A type's SQL name, as `format_type` prints it.
pub(crate) fn type_name(oid: pg_sys::Oid) -> String {
    // SAFETY: format_type_be returns a palloc'd NUL-terminated string for
    // any oid (or raises an ERROR).
    unsafe { CStr::from_ptr(pg_sys::format_type_be(oid)) }.to_string_lossy().into_owned()
}

/// Passes every remaining row of `cursor` to `each`, a batch at a time,
/// checking for interrupts between batches and freeing each batch after
/// it. Each row is a [`Row`] of its batch's tuple table, so that its id
/// and text columns can be read in place, without the copy pgrx's typed
/// getters make into a memory context that lives until SPI finishes (one
/// allocation per row, held for the whole read). Rows are read in a memory
/// context of their own, which holds whatever reading them detoasts: it is
/// reset after each batch, and within one once it holds [`RESET_BYTES`].
pub(crate) fn for_each_tuple<E: From<pgrx::spi::Error>>(
    cursor: &mut SpiCursor<'_>,
    mut each: impl FnMut(&Row<'_>) -> Result<(), E>,
) -> Result<(), E> {
    let mut rows = PgMemoryContexts::new("warren rows");
    let context = rows.value();
    loop {
        check_for_interrupts!();
        let mut batch = cursor.fetch(BATCH_ROWS)?;
        if batch.is_empty() {
            return Ok(());
        }
        // SAFETY: `rows` is a context this function owns. What is allocated
        // in it while a row is read is reachable only through that `Row`,
        // whose borrow ends with the call of `each` that it is passed to,
        // before the context is reset, here or below.
        let read = unsafe {
            rows.switch_to(|_| {
                while batch.next().is_some() {
                    each(&Row { table: &batch })?;
                    if pg_sys::MemoryContextMemAllocated(context, true) as u64 > RESET_BYTES {
                        pg_sys::MemoryContextReset(context);
                    }
                }
                Ok::<_, E>(())
            })
        };
        // SAFETY: nothing allocated in `rows` is used after the batch.
        unsafe { rows.reset() };
        read?;
        // SAFETY: SPI_tuptable is the batch just consumed, which nothing
        // refers to any more.
        unsafe { pg_sys::SPI_freetuptable(pg_sys::SPI_tuptable) };
    }
}

/// A row of a cursor, as [`for_each_tuple`] passes it: its batch's tuple
/// table positioned at it, for one call. What is read from it in place
/// borrows the row, so it cannot outlive the call, after which the memory
/// context that holds what reading it detoasted may be reset. Its typed
/// getters (through `Deref`) copy what they return.
pub(crate) struct Row<'r> {
    table: &'r SpiTupleTable<'r>,
}

impl<'r> Row<'r> {
    /// The external id in column `ordinal` (of a type `E` reads), read in
    /// place as [`Row::text`] reads a `text`.
    pub(crate) fn id<E: IdDatum>(
        &self,
        ordinal: usize,
    ) -> Result<Option<E::InPlace<'_>>, pgrx::spi::Error> {
        let Some(datum) = self.table.get_datum_by_ordinal(ordinal)? else {
            return Ok(None);
        };
        // SAFETY: `datum` is the non-NULL id (the caller checked the
        // column's type) of the tuple the table is at, which lives until the
        // tuple table is freed; detoasted, it lives in the rows' memory
        // context, which is not reset while the row is borrowed.
        Ok(unsafe { E::in_place(datum, false) })
    }

    /// The `text` in column `ordinal`, read in place from the tuple (or, if
    /// stored out of line or compressed, detoasted into the rows' memory
    /// context), without a copy.
    pub(crate) fn text(&self, ordinal: usize) -> Result<Option<&str>, pgrx::spi::Error> {
        let Some(datum) = self.table.get_datum_by_ordinal(ordinal)? else {
            return Ok(None);
        };
        // SAFETY: as for `id`: the non-NULL `text` of the tuple, which lives
        // while the row is borrowed.
        Ok(unsafe { <&str as FromDatum>::from_polymorphic_datum(datum, false, pg_sys::TEXTOID) })
    }
}

impl<'r> std::ops::Deref for Row<'r> {
    type Target = SpiTupleTable<'r>;

    fn deref(&self) -> &SpiTupleTable<'r> {
        self.table
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;

    use super::*;

    /// Reading a cursor in a test failed.
    #[derive(Debug)]
    struct ReadError;

    impl From<SourceError> for ReadError {
        fn from(_: SourceError) -> Self {
            Self
        }
    }

    impl From<pgrx::spi::Error> for ReadError {
        fn from(_: pgrx::spi::Error) -> Self {
            Self
        }
    }

    /// Ids detoasted while a batch is read are freed as it is read, not
    /// only once it ends: 100 rows of a compressed 1 MB text, all in one
    /// batch, never hold more than the reset threshold and a row.
    #[pg_test]
    fn detoasted_rows_are_freed_within_a_batch() -> Result<(), pgrx::spi::Error> {
        Spi::run(
            "CREATE TABLE big_ids AS \
             SELECT repeat(md5(n::text), 32768) AS id FROM generate_series(1, 100) n",
        )?;
        let read = with_pinned_snapshot(|| {
            Spi::connect(|client| {
                let mut opened =
                    open_read_only::<ReadError>(client, "query", "SELECT id FROM big_ids")?;
                let (mut rows, mut most) = (0, 0);
                for_each_tuple::<ReadError>(&mut opened.cursor, |row| {
                    assert_eq!(row.text(1)?.map(str::len), Some(1 << 20));
                    // SAFETY: reads the size of the current context.
                    let held = unsafe {
                        pg_sys::MemoryContextMemAllocated(pg_sys::CurrentMemoryContext, true)
                    };
                    rows += 1;
                    most = most.max(held);
                    Ok(())
                })?;
                Ok::<_, ReadError>((rows, most))
            })
        });
        assert!(read.is_ok(), "{read:?}");
        let (rows, most) = read.unwrap_or_default();
        assert_eq!(rows, 100);
        assert!(most < (RESET_BYTES + (2 << 20)) as usize, "{most} bytes held");
        Ok(())
    }
}
