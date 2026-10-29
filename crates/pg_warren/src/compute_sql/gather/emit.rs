//! `gather`'s result rows, streamed: the stream owns the kernel's result
//! (still charged to the call's budget) and builds each receiver's row
//! when the executor asks for it, so only the current row is palloc'd.
//! A row is the receiver's external id and its combined value as a datum
//! of the result type (an array built as `array_agg` builds it, for
//! `collect`: payloads are laid out straight from the kernel's bytes, every
//! other message type is accumulated a datum at a time).

use std::mem::size_of;
use std::ptr;
use std::sync::Arc;

use pgrx::prelude::*;
use pgrx::{AnyElement, IntoDatum, PgSqlErrorCode};
use warren_core::compute::{Bytes, Collected, Gathered, Message, Received, RowAt};

use crate::cache::{CachedIds, LoadedVersion};
use crate::external_ids::external;

/// A row of the result.
pub(super) type Row = (AnyElement, AnyElement);

/// Makes a single value of a message type as a datum of the given type.
pub(super) type Build<M> =
    Box<dyn for<'a> FnMut(<M as Message>::View<'a>, pg_sys::Oid) -> Option<pg_sys::Datum>>;

/// Makes a result's values of messages of type `M` as datums.
pub(super) trait Values<M: Message + 'static> {
    /// A single message as a datum of type `sql`.
    fn single(&mut self, message: M::View<'_>, sql: pg_sys::Oid) -> Option<pg_sys::Datum>;

    /// A receiver's collected `messages` as an array of type `element`'s
    /// elements, accumulated as `array_agg` accumulates them: a
    /// multidimensional array for array elements, which must then all have
    /// the same dimensions. `None` if an element could not be made.
    fn array(&mut self, messages: Collected<'_, M>, element: pg_sys::Oid) -> Option<pg_sys::Datum> {
        let mut state = ptr::null_mut();
        for view in messages.iter() {
            let value = self.single(view, element)?;
            // SAFETY: each datum is a non-NULL value of type `element`; the
            // build state lives in the current memory context, which
            // outlives the row.
            state = unsafe {
                pg_sys::accumArrayResultAny(
                    state,
                    value,
                    false,
                    element,
                    pg_sys::CurrentMemoryContext,
                )
            };
        }
        // SAFETY: `state` is the build state made above, released here.
        (!state.is_null()).then(|| unsafe {
            pg_sys::makeArrayResultAny(state, pg_sys::CurrentMemoryContext, true)
        })
    }
}

/// Values a [`Build`] makes, arrays of them accumulated.
pub(super) struct Accumulated<M: Message>(pub(super) Build<M>);

impl<M: Message + 'static> Values<M> for Accumulated<M> {
    fn single(&mut self, message: M::View<'_>, sql: pg_sys::Oid) -> Option<pg_sys::Datum> {
        (self.0)(message, sql)
    }
}

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
    /// Makes the values.
    pub(super) values: Box<dyn Values<M>>,
    /// The next row.
    pub(super) at: RowAt,
}

impl<E: CachedIds, M: Message + 'static> Iterator for Stream<E, M> {
    type Item = Row;

    fn next(&mut self) -> Option<Row> {
        let Self { loaded, gathered, result, message, values, at } = self;
        let (node, received) = gathered.next_row(at)?;
        let value = match received {
            Received::Count(count) => count.into_datum(),
            Received::Message(view) => values.single(view, *result),
            Received::Messages(views) => values.array(views, *message),
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

/// An `i64` message as a datum of integer type `sql` (`smallint` and
/// `integer` messages were widened to `bigint`, so they fit).
pub(super) fn integer(value: i64, sql: pg_sys::Oid) -> Option<pg_sys::Datum> {
    match sql {
        pg_sys::INT2OID => i16::try_from(value).ok()?.into_datum(),
        pg_sys::INT4OID => i32::try_from(value).ok()?.into_datum(),
        _ => value.into_datum(),
    }
}

/// `text`, `varchar`, `char` or `bytea` payloads (text in the database
/// encoding, unchecked) as datums: each is a varlena of its bytes.
pub(super) struct Payloads;

/// Postgres's `MaxAllocSize`: the largest array it builds.
const MAX_ALLOC_SIZE: usize = 0x3fff_ffff;

/// The most elements `array_agg` accumulates: its datums, an array grown
/// by doubling from 64, must stay within [`MAX_ALLOC_SIZE`].
const MAX_ELEMENTS: usize = 1 << 26;

/// The bytes of a one-dimensional array without NULLs before its first
/// element: the header and one dimension and lower bound, max-aligned.
const ARRAY_HEADER: usize = (size_of::<pg_sys::ArrayType>() + 2 * size_of::<i32>() + 7) & !7;

/// The bytes an element of `len` payload bytes takes in an array: a
/// 4-byte varlena header and the bytes, int-aligned (`text`, `varchar`,
/// `char` and `bytea` all align so).
fn element_size(len: usize) -> Option<usize> {
    Some(len.checked_add(pg_sys::VARHDRSZ + 3)? & !3)
}

/// The bytes of the array of `count` payloads of `lengths`, or `None` past
/// what `array_agg` builds.
fn payload_array_size(count: usize, lengths: impl Iterator<Item = usize>) -> Option<usize> {
    if count > MAX_ELEMENTS {
        return None;
    }
    lengths
        .map(element_size)
        .try_fold(ARRAY_HEADER, |size, element| size.checked_add(element?))
        .filter(|&size| size <= MAX_ALLOC_SIZE)
}

impl Values<Bytes> for Payloads {
    fn single(&mut self, bytes: &[u8], _: pg_sys::Oid) -> Option<pg_sys::Datum> {
        let len = i32::try_from(bytes.len()).ok()?;
        // SAFETY: cstring_to_text_with_len copies `len` bytes into a new
        // varlena, which is also how the other payload types are laid out.
        let varlena = unsafe { pg_sys::cstring_to_text_with_len(bytes.as_ptr().cast(), len) };
        Some(pg_sys::Datum::from(varlena))
    }

    fn array(
        &mut self,
        messages: Collected<'_, Bytes>,
        element: pg_sys::Oid,
    ) -> Option<pg_sys::Datum> {
        payload_array(messages, element)
    }
}

/// The array of payloads `messages` of element type `element`, laid out
/// as `array_agg` lays it out (one dimension from 1, no NULL bitmap, each
/// element a 4-byte varlena header and its bytes, int-aligned, padding
/// zeroed), in one allocation straight from the kernel's bytes. An array
/// past `MaxAllocSize` is the error `array_agg` raises (54000).
fn payload_array(messages: Collected<'_, Bytes>, element: pg_sys::Oid) -> Option<pg_sys::Datum> {
    if messages.is_empty() {
        return None;
    }
    let Some(size) = payload_array_size(messages.len(), messages.iter().map(<[u8]>::len)) else {
        ereport!(
            ERROR,
            PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
            format!("array size exceeds the maximum allowed ({MAX_ALLOC_SIZE})")
        );
    };
    let (count, varsize) = (i32::try_from(messages.len()).ok()?, i32::try_from(size).ok()?);
    // SAFETY: a zeroed allocation of `size` bytes, the header and every
    // element's int-aligned varlena, written within it in order; each
    // varlena's length fits an i32, as the array's does.
    unsafe {
        let array = pg_sys::palloc0(size).cast::<u8>();
        pgrx::varlena::set_varsize_4b(array.cast(), varsize);
        let header = array.cast::<pg_sys::ArrayType>();
        (*header).ndim = 1;
        (*header).dataoffset = 0;
        (*header).elemtype = element;
        let bounds = array.add(size_of::<pg_sys::ArrayType>()).cast::<i32>();
        bounds.write(count);
        bounds.add(1).write(1);
        let mut at = array.add(ARRAY_HEADER);
        for bytes in messages.iter() {
            let len = pg_sys::VARHDRSZ + bytes.len();
            pgrx::varlena::set_varsize_4b(at.cast(), i32::try_from(len).ok()?);
            ptr::copy_nonoverlapping(bytes.as_ptr(), at.add(pg_sys::VARHDRSZ), bytes.len());
            at = at.add(element_size(bytes.len())?);
        }
        Some(pg_sys::Datum::from(array))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_arrays_are_laid_out_as_array_agg_lays_them_out() {
        assert_eq!(ARRAY_HEADER, 24);
        assert_eq!([0, 1, 4, 5].map(element_size), [4, 8, 8, 12].map(Some));
        assert_eq!(payload_array_size(4, [0, 1, 4, 5].into_iter()), Some(24 + 32));
        assert_eq!(payload_array_size(0, [].into_iter()), Some(24));
    }

    #[test]
    fn a_payload_array_past_what_array_agg_builds_has_no_size() {
        let one = |len: usize| payload_array_size(1, [len].into_iter());
        assert_eq!(one(MAX_ALLOC_SIZE - 31), Some(MAX_ALLOC_SIZE - 3));
        assert_eq!(one(MAX_ALLOC_SIZE - 30), None);
        assert_eq!(one(usize::MAX), None);
        let empty = |count| payload_array_size(count, std::iter::repeat_n(0, count));
        assert_eq!(empty(MAX_ELEMENTS), Some(24 + (4 << 26)));
        assert_eq!(empty(MAX_ELEMENTS + 1), None);
    }
}
