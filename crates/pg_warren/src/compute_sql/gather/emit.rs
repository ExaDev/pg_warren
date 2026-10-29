//! `gather`'s result rows, streamed: the stream owns the kernel's result
//! (still charged to the call's budget) and builds each receiver's row
//! when the executor asks for it, so only the current row is palloc'd.
//! A row is the receiver's external id and its combined value as a datum
//! of the result type (an array built as `array_agg` builds it, for
//! `collect`).

use std::ptr;
use std::sync::Arc;

use pgrx::prelude::*;
use pgrx::{AnyElement, IntoDatum, PgSqlErrorCode};
use warren_core::compute::{Bytes, Gathered, Message, Received, RowAt};

use crate::cache::{CachedIds, LoadedVersion};
use crate::external_ids::external;

/// A row of the result.
pub(super) type Row = (AnyElement, AnyElement);

/// Makes a single value of a message type as a datum of the given type.
pub(super) type Build<M> =
    Box<dyn for<'a> FnMut(<M as Message>::View<'a>, pg_sys::Oid) -> Option<pg_sys::Datum>>;

/// The rows of a gather, built one at a time.
pub(super) struct Stream<E: CachedIds, M: Message> {
    /// The loaded version, for external ids.
    pub(super) loaded: Arc<LoadedVersion<E>>,
    /// The kernel's result.
    pub(super) gathered: Gathered<M>,
    /// The result type.
    pub(super) result: pg_sys::Oid,
    /// The messages' type: a collected array's element type.
    pub(super) message: pg_sys::Oid,
    /// Makes single values.
    pub(super) build: Build<M>,
    /// The next row.
    pub(super) at: RowAt,
}

impl<E: CachedIds, M: Message> Iterator for Stream<E, M> {
    type Item = Row;

    fn next(&mut self) -> Option<Row> {
        let Self { loaded, gathered, result, message, build, at } = self;
        let (node, received) = gathered.next_row(at)?;
        let value = match received {
            Received::Count(count) => count.into_datum(),
            Received::Message(view) => build(view, *result),
            Received::Messages(views) => {
                array(views.iter().map(|view| build(view, *message)), *message)
            }
        };
        // SAFETY: `value` was made as a datum of type `result`.
        let value = value
            .and_then(|value| unsafe { AnyElement::from_polymorphic_datum(value, false, *result) });
        match external(&loaded.id_map, node).zip(value) {
            Some(row) => Some(row),
            None => {
                ereport!(
                    ERROR,
                    PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
                    format!("gather could not build the result row of dense id {node}")
                );
            }
        }
    }
}

/// The array of `elements`, of SQL type `element`, as `array_agg` makes it:
/// a multidimensional array for array elements, which must then all have
/// the same dimensions. `None` if an element could not be made.
fn array(
    elements: impl Iterator<Item = Option<pg_sys::Datum>>,
    element: pg_sys::Oid,
) -> Option<pg_sys::Datum> {
    let mut state = ptr::null_mut();
    for value in elements {
        // SAFETY: each datum is a non-NULL value of type `element`; the
        // build state lives in the current memory context, which outlives
        // the row.
        state = unsafe {
            pg_sys::accumArrayResultAny(state, value?, false, element, pg_sys::CurrentMemoryContext)
        };
    }
    // SAFETY: `state` is the build state made above, released here.
    (!state.is_null())
        .then(|| unsafe { pg_sys::makeArrayResultAny(state, pg_sys::CurrentMemoryContext, true) })
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

/// `text`, `varchar`, `char` or `bytea` bytes (text in the database
/// encoding, unchecked) as a datum: each is a varlena of the bytes.
pub(super) fn payload(bytes: &[u8]) -> Option<pg_sys::Datum> {
    let len = i32::try_from(bytes.len()).ok()?;
    // SAFETY: cstring_to_text_with_len copies `len` bytes into a new
    // varlena, which is also how a bytea is laid out.
    let varlena = unsafe { pg_sys::cstring_to_text_with_len(bytes.as_ptr().cast(), len) };
    Some(pg_sys::Datum::from(varlena))
}

/// Rebuilds values of a type from their binary send form with its
/// receive function, through one buffer reused for every value.
pub(super) fn receiver(receive: pg_sys::Oid, ioparam: pg_sys::Oid) -> Build<Bytes> {
    // SAFETY: a fresh StringInfo in the current memory context: the SRF's
    // multi-call context on its first call, which outlives every row.
    let buffer = unsafe { pg_sys::makeStringInfo() };
    Box::new(move |bytes: &[u8], _| {
        let len = i32::try_from(bytes.len()).ok()?;
        // SAFETY: `buffer` is the live StringInfo made above, refilled with
        // a copy of `bytes`, which came from the same type's send
        // function; the receive function raises an ERROR on a bad buffer.
        unsafe {
            pg_sys::resetStringInfo(buffer);
            pg_sys::appendBinaryStringInfo(buffer, bytes.as_ptr().cast(), len);
            Some(pg_sys::OidReceiveFunctionCall(receive, buffer, ioparam, -1))
        }
    })
}
