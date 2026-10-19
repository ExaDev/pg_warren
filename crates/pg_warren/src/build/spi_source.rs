//! Source queries over SPI: opened as read-only cursors, which also checks
//! that each is a single SELECT that writes nothing (DESIGN.md §5.4 step 1).

use std::ffi::{CStr, CString};
use std::ptr;

use pgrx::prelude::*;
use pgrx::spi::{SpiClient, SpiCursor};
use pgrx::PgTupleDesc;

use super::source::{ColumnType, SourceError};

/// The result column types of `query`, opened (not run) as a read-only
/// cursor as the current user.
pub(crate) fn source_columns<E>(name: &'static str, query: &str) -> Result<Vec<ColumnType>, E>
where
    E: From<SourceError> + From<pgrx::spi::Error>,
{
    Spi::connect(|client| open_read_only(client, name, query).map(|(_, columns)| columns))
}

/// Opens `query` as a read-only cursor under the active snapshot and
/// returns it with its result column types. Postgres raises an ERROR for a
/// query that cannot be a cursor (not a single SELECT, or not read-only).
fn open_read_only<'c, E>(
    client: &SpiClient<'c>,
    name: &'static str,
    query: &str,
) -> Result<(SpiCursor<'c>, Vec<ColumnType>), E>
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
    // NUL-terminated string and its tuple descriptor, set by PortalStart for
    // a SELECT, lives as long as the portal.
    let (cursor_name, columns) = unsafe {
        (
            CStr::from_ptr((*portal).name).to_string_lossy().into_owned(),
            column_types((*portal).tupDesc),
        )
    };
    Ok((client.find_cursor(&cursor_name)?, columns))
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
