//! Reading `gather`'s messages into the kernel's buffers: from a vector
//! (every live node's value), or from a query's `(id, message)` rows run as
//! the caller, under the caller's `search_path`, as a read-only cursor.
//! Rows whose id is NULL, unknown or tombstoned in the version, or whose
//! message is NULL, send nothing.

use pgrx::prelude::*;
use pgrx::spi::SpiHeapTupleData;
use pgrx::{AnyElement, FromDatum, IntoDatum};
use warren_core::compute::{Bytes, Element, Messages};
use warren_core::graph::DenseId;

use super::super::vectors::Vectors;
use super::super::ComputeSqlError;
use crate::access::{with_search_path, Caller};
use crate::source::{for_each_row, open_read_only, IdDatum};

/// Every live node's value of `vector` (width 1), as a message.
pub(super) fn from_vector<E: IdDatum, T: Element>(
    vectors: &Vectors<E>,
    values: &[T],
    messages: &mut Messages<T>,
) -> Result<(), ComputeSqlError> {
    let tombstones = vectors.tombstones();
    for (node, &value) in (0..).zip(values) {
        if node % (1 << 16) == 0 {
            check_for_interrupts!();
        }
        if !tombstones.contains(node) {
            messages.push(node, value)?;
        }
    }
    Ok(())
}

/// Passes each row of `query` (columns `id`, `value`) whose id is a live
/// node of the version to `each`, with its dense id.
pub(super) fn from_query<E: IdDatum>(
    vectors: &Vectors<E>,
    caller: &Caller,
    query: &str,
    mut each: impl FnMut(DenseId, &SpiHeapTupleData<'_>) -> Result<(), ComputeSqlError>,
) -> Result<(), ComputeSqlError> {
    with_search_path(caller.search_path(), || {
        Spi::connect(|client| {
            let mut opened = open_read_only::<ComputeSqlError>(client, "messages", query)?;
            for_each_row(&mut opened.cursor, |row| {
                let Some(id) = row.get::<E::Sql>(1)?.map(E::from_sql) else {
                    return Ok(());
                };
                match vectors.loaded.id_map.dense_id(&id) {
                    Some(node) => each(node, row),
                    None => Ok(()),
                }
            })
        })
    })
}

/// The non-NULL message of `row`, read as `T`, pushed from `node`.
pub(super) fn element<T: Element + FromDatum + IntoDatum>(
    messages: &mut Messages<T>,
    node: DenseId,
    row: &SpiHeapTupleData<'_>,
) -> Result<(), ComputeSqlError> {
    if let Some(value) = row.get::<T>(2)? {
        messages.push(node, value)?;
    }
    Ok(())
}

/// The non-NULL `text` message of `row`, as its bytes.
pub(super) fn text(
    messages: &mut Messages<Bytes>,
    node: DenseId,
    row: &SpiHeapTupleData<'_>,
) -> Result<(), ComputeSqlError> {
    if let Some(value) = row.get::<String>(2)? {
        messages.push(node, value.as_bytes())?;
    }
    Ok(())
}

/// The non-NULL `bytea` message of `row`, as its bytes.
pub(super) fn bytea(
    messages: &mut Messages<Bytes>,
    node: DenseId,
    row: &SpiHeapTupleData<'_>,
) -> Result<(), ComputeSqlError> {
    if let Some(value) = row.get::<Vec<u8>>(2)? {
        messages.push(node, &value)?;
    }
    Ok(())
}

/// The non-NULL message of `row` in its type's binary send form, by send
/// function `send`.
pub(super) fn binary(
    send: pg_sys::Oid,
    messages: &mut Messages<Bytes>,
    node: DenseId,
    row: &SpiHeapTupleData<'_>,
) -> Result<(), ComputeSqlError> {
    let Some(value) = row.get::<AnyElement>(2)? else {
        return Ok(());
    };
    // SAFETY: `send` is the binary send function of the value's type; it
    // returns a palloc'd bytea, read and freed here.
    unsafe {
        let sent = pg_sys::OidSendFunctionCall(send, value.datum());
        let pushed = messages.push(node, pgrx::varlena::varlena_to_byte_slice(sent));
        pg_sys::pfree(sent.cast());
        pushed?;
    }
    Ok(())
}
