//! `gather`'s result rows: each receiver's external id, and its combined
//! value as a datum of the result type (an array built as `array_agg`
//! builds it, for `collect`).

use std::ptr;

use pgrx::prelude::*;
use pgrx::{AnyElement, FromDatum, IntoDatum};
use warren_core::compute::{Gathered, Message, Received};
use warren_core::graph::IdMap;

use crate::external_ids::external;
use crate::source::IdDatum;

/// A row of the result.
pub(super) type Row = (AnyElement, AnyElement);

/// The rows of `gathered`, whose single values `datum` makes as a datum of
/// a given type: `result` for one value, `message` for each element of a
/// collected array.
pub(super) fn rows<E: IdDatum, M: Message>(
    id_map: &IdMap<E>,
    gathered: &Gathered<M>,
    result: pg_sys::Oid,
    message: pg_sys::Oid,
    datum: impl Fn(M::View<'_>, pg_sys::Oid) -> Option<pg_sys::Datum>,
) -> Vec<Row> {
    let mut rows = Vec::with_capacity(gathered.len());
    for (node, received) in gathered.iter() {
        let value = match received {
            Received::Count(count) => count.into_datum(),
            Received::Message(view) => datum(view, result),
            Received::Messages(views) => {
                array(views.iter().filter_map(|v| datum(v, message)), message)
            }
        };
        let row = external(id_map, node).zip(value.and_then(|value| {
            // SAFETY: `value` was made as a datum of type `result`.
            unsafe { AnyElement::from_polymorphic_datum(value, false, result) }
        }));
        rows.extend(row);
    }
    rows
}

/// The array of `elements`, of SQL type `element`, as `array_agg` makes it:
/// a multidimensional array for array elements, which must then all have
/// the same dimensions.
fn array(
    elements: impl Iterator<Item = pg_sys::Datum>,
    element: pg_sys::Oid,
) -> Option<pg_sys::Datum> {
    let mut state = ptr::null_mut();
    // SAFETY: each datum is a non-NULL value of type `element`; the build
    // state lives in the current memory context, which outlives the call.
    unsafe {
        for value in elements {
            state = pg_sys::accumArrayResultAny(
                state,
                value,
                false,
                element,
                pg_sys::CurrentMemoryContext,
            );
        }
        (!state.is_null())
            .then(|| pg_sys::makeArrayResultAny(state, pg_sys::CurrentMemoryContext, true))
    }
}

/// An `i64` message as a datum of integer type `sql` (`smallint` and
/// `integer` messages were widened to `bigint`, so they fit).
pub(super) fn integer(value: i64, sql: pg_sys::Oid) -> Option<pg_sys::Datum> {
    match sql {
        pg_sys::INT2OID => i16::try_from(value).ok()?.into_datum(),
        pg_sys::INT4OID => i32::try_from(value).ok()?.into_datum(),
        _ => value.into_datum(),
    }
}

/// `text` or `bytea` bytes as a datum of `sql`.
pub(super) fn payload(bytes: &[u8], sql: pg_sys::Oid) -> Option<pg_sys::Datum> {
    match sql {
        pg_sys::BYTEAOID => bytes.into_datum(),
        _ => std::str::from_utf8(bytes).ok()?.into_datum(),
    }
}

/// A value of the type with binary receive function `receive` (and I/O
/// parameter `ioparam`), rebuilt from its send form.
pub(super) fn received(
    bytes: &[u8],
    receive: pg_sys::Oid,
    ioparam: pg_sys::Oid,
) -> Option<pg_sys::Datum> {
    let len = i32::try_from(bytes.len()).ok()?;
    // SAFETY: the buffer is a fresh StringInfo holding a copy of `bytes`,
    // which `bytes` came from the same type's send function; the receive
    // function raises an ERROR on a bad buffer.
    unsafe {
        let buffer = pg_sys::makeStringInfo();
        pg_sys::appendBinaryStringInfo(buffer, bytes.as_ptr().cast(), len);
        Some(pg_sys::OidReceiveFunctionCall(receive, buffer, ioparam, -1))
    }
}
