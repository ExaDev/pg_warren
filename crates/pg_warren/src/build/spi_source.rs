//! The SPI adapter of the source port: the node and edge queries run as
//! read-only cursors under the snapshot that [`with_pinned_snapshot`] pins,
//! as the graph owner (DESIGN.md §5.4 step 1, §7).

use std::ffi::{CStr, CString};
use std::ptr;

use pgrx::prelude::*;
use pgrx::spi::{SpiClient, SpiCursor, SpiHeapTupleData};
use pgrx::PgTupleDesc;
use warren_core::graph::GraphError;

use super::error::BuildError;
use super::source::{
    check_edge_columns, check_node_columns, ColumnType, EdgeRow, SourceError, SourceReader,
};
use crate::catalog::{as_role, GraphDefinition};

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
    Spi::connect(|client| open_read_only(client, name, query).map(|(_, columns)| columns))
}

/// A graph's sources, read through SPI.
pub(crate) struct SpiSource<'g> {
    graph: &'g GraphDefinition,
}

impl<'g> SpiSource<'g> {
    /// The sources of `graph`, read as its owner.
    pub(crate) fn new(graph: &'g GraphDefinition) -> Self {
        Self { graph }
    }
}

impl SourceReader<i64> for SpiSource<'_> {
    fn node_ids(&mut self) -> Result<Vec<i64>, BuildError> {
        let graph = self.graph;
        as_role(graph.owner, || {
            Spi::connect(|client| {
                let (mut cursor, columns) =
                    open_read_only::<BuildError>(client, "node_query", &graph.node_query)?;
                check_node_columns(&columns)?;
                let mut ids = Vec::new();
                for_each_row(&mut cursor, |row| {
                    if let Some(id) = row.get::<i64>(1)? {
                        ids.try_reserve(1)
                            .map_err(|_| GraphError::OutOfMemory { elements: ids.len() + 1 })?;
                        ids.push(id);
                    }
                    Ok(())
                })?;
                Ok(ids)
            })
        })
    }

    fn edges(
        &mut self,
        each: &mut dyn FnMut(EdgeRow<'_, i64>) -> Result<(), BuildError>,
    ) -> Result<(), BuildError> {
        let graph = self.graph;
        let weight_column = if graph.has_relations { 4 } else { 3 };
        as_role(graph.owner, || {
            Spi::connect(|client| {
                let (mut cursor, columns) =
                    open_read_only::<BuildError>(client, "edge_query", &graph.edge_query)?;
                check_edge_columns(&columns, graph.has_relations, graph.weighted)?;
                for_each_row(&mut cursor, |row| {
                    each(EdgeRow {
                        source: row.get::<i64>(1)?,
                        target: row.get::<i64>(2)?,
                        relation: match graph.has_relations {
                            true => row.get::<&str>(3)?,
                            false => None,
                        },
                        weight: match graph.weighted {
                            true => row.get::<f64>(weight_column)?,
                            false => None,
                        },
                    })
                })
            })
        })
    }
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

/// Passes every remaining row of `cursor` to `each`, a batch at a time,
/// checking for interrupts between batches and freeing each batch after it.
fn for_each_row(
    cursor: &mut SpiCursor<'_>,
    mut each: impl FnMut(&SpiHeapTupleData<'_>) -> Result<(), BuildError>,
) -> Result<(), BuildError> {
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
