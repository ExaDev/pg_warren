//! External ids in and out of SQL functions: polymorphic (`anyelement`,
//! `anyarray`) arguments checked against the graph's id type, mapped to
//! dense ids through the version's id map, and results mapped back
//! (DESIGN.md §5.2, §9.3, §14 item 1). Used by every feature that takes or
//! returns node ids (`traversal_sql`, `compute_sql`).
//!
//! Calls that map many ids map them through `lookup::DenseIds`, which
//! indexes a text id map once a call has mapped enough of them.
//!
//! An external id the version does not know, or knows only as tombstoned,
//! is not a node of that version: it is skipped as a start or filter
//! member, and a lookup of it finds nothing. NULL ids are skipped too.

mod error;
mod lookup;

pub(crate) use error::ExternalIdError;
pub(crate) use lookup::DenseIds;

use std::borrow::Borrow;

use pgrx::prelude::*;
use pgrx::{AnyArray, AnyElement, Array, FromDatum};
use warren_core::graph::{DenseId, IdMap};

use crate::source::{type_name, IdDatum};

/// The external id `value` holds, checked to be of the graph's id type.
pub(crate) fn element<E: IdDatum>(
    graph: &str,
    argument: &'static str,
    value: AnyElement,
) -> Result<E::Owned, ExternalIdError> {
    check_type::<E>(graph, argument, value.oid(), false)?;
    owned::<E>(value).ok_or(ExternalIdError::Null(argument))
}

/// The id `value` holds, which is of a type `E` reads.
fn owned<E: IdDatum>(value: AnyElement) -> Option<E::Owned> {
    // SAFETY: the caller checked that the datum's type is one E reads; it
    // lives for the call.
    unsafe { E::in_place(value.datum(), false) }.map(|id| E::owned(id.borrow()))
}

/// [`element`], but for lookups whose result type does not depend on the
/// argument's, a narrower type is also accepted: an `int2` or `int4` value
/// for a wider integer id type, so `warren.degree('g', 42)` works on a
/// `bigint` graph, and a `varchar` for `text`.
pub(crate) fn element_widened<E: IdDatum>(
    graph: &str,
    argument: &'static str,
    value: AnyElement,
) -> Result<E::Owned, ExternalIdError> {
    if value.oid() != E::ID_TYPE.oid() && E::ID_TYPE.reads(value.oid()) {
        return owned::<E>(value).ok_or(ExternalIdError::Null(argument));
    }
    // SAFETY: each read below is of the datum's own type, `value.oid()`.
    let narrow = unsafe {
        match value.oid() {
            pg_sys::INT2OID => {
                i16::from_polymorphic_datum(value.datum(), false, value.oid()).map(i64::from)
            }
            pg_sys::INT4OID if value.oid() != E::ID_TYPE.oid() => {
                i32::from_polymorphic_datum(value.datum(), false, value.oid()).map(i64::from)
            }
            _ => None,
        }
    };
    match narrow.and_then(E::from_integer) {
        Some(id) => Ok(id),
        None => element::<E>(graph, argument, value),
    }
}

/// The non-NULL external ids `array` holds, checked to be an array of the
/// graph's id type.
pub(crate) fn elements<E: IdDatum>(
    graph: &str,
    argument: &'static str,
    array: AnyArray,
) -> Result<Vec<E::Owned>, ExternalIdError> {
    check_type::<E>(graph, argument, array.oid(), true)?;
    // SAFETY: `check_type` found an array of a type E reads.
    let ids = datums(&array)
        .into_iter()
        .flatten()
        .filter_map(|datum| unsafe { E::in_place(datum, false) });
    Ok(ids.map(|id| E::owned(id.borrow())).collect())
}

/// The elements of `array`, NULL as `None`, read through pgrx's
/// by-reference iterator: its by-value one, behind `&AnyArray`, asserts
/// that its data pointer is in bounds before it checks for NULL, which
/// fails (in debug builds) on a trailing NULL element.
pub(crate) fn datums(array: &AnyArray) -> Vec<Option<pg_sys::Datum>> {
    // SAFETY: an `anyarray` (or array `"any"`) argument's datum is an array
    // of its own type, which lives for the call.
    let read = unsafe {
        Array::<pg_sys::Datum>::from_polymorphic_datum(array.datum(), false, array.oid())
    };
    read.map(|read| read.iter().collect()).unwrap_or_default()
}

/// The dense ids of the live nodes among `ids`, in order.
pub(crate) fn dense_ids<E: IdDatum>(id_map: &IdMap<E>, ids: &[E::Owned]) -> Vec<DenseId> {
    ids.iter().filter_map(|id| id_map.dense_id(id.borrow())).collect()
}

/// The external id of `dense` as a SQL value of the graph's id type.
pub(crate) fn external<E: IdDatum>(id_map: &IdMap<E>, dense: DenseId) -> Option<AnyElement> {
    let datum = E::datum(id_map.external_id(dense)?)?;
    // SAFETY: the datum was just made of E's id type.
    unsafe { AnyElement::from_polymorphic_datum(datum, false, E::ID_TYPE.oid()) }
}

/// The external ids of `path` as a SQL array of the graph's id type.
pub(crate) fn external_array<E: IdDatum>(id_map: &IdMap<E>, path: &[DenseId]) -> Option<AnyArray> {
    let mut ids: Vec<pg_sys::Datum> =
        path.iter().filter_map(|&dense| id_map.external_id(dense)).filter_map(E::datum).collect();
    let oid = E::ID_TYPE.oid();
    let (mut len, mut by_value, mut align) = (0, false, 0);
    let count = i32::try_from(ids.len()).ok()?;
    // SAFETY: get_typlenbyvalalign and get_array_type only read the type
    // cache; construct_array copies `count` datums of type `oid`, laid out
    // as it says, into a new array, raising an ERROR rather than return
    // NULL.
    let (array, array_type) = unsafe {
        pg_sys::get_typlenbyvalalign(oid, &mut len, &mut by_value, &mut align);
        let array =
            pg_sys::construct_array(ids.as_mut_ptr(), count, oid, len.into(), by_value, align);
        (array, pg_sys::get_array_type(oid))
    };
    // SAFETY: the datum was just made as an array of E's id type.
    unsafe { AnyArray::from_polymorphic_datum(pg_sys::Datum::from(array), false, array_type) }
}

/// The element type of array type `oid`; an error naming `argument` if
/// `oid` is not an array type (`example` is the id type to suggest).
pub(crate) fn element_type(
    argument: &'static str,
    oid: pg_sys::Oid,
    example: &str,
) -> Result<pg_sys::Oid, ExternalIdError> {
    // SAFETY: get_element_type only reads the type cache.
    let element = unsafe { pg_sys::get_element_type(oid) };
    if element == pg_sys::Oid::INVALID {
        return Err(ExternalIdError::NotAnArray {
            argument,
            id_type: example.to_owned(),
            found: type_name(oid),
        });
    }
    Ok(element)
}

/// Checks that `oid` is the graph's id type (`array`: an array of it).
pub(crate) fn check_type<E: IdDatum>(
    graph: &str,
    argument: &'static str,
    oid: pg_sys::Oid,
    array: bool,
) -> Result<(), ExternalIdError> {
    let expected = E::ID_TYPE.oid();
    let found = match array {
        true => element_type(argument, oid, &type_name(expected))?,
        false => oid,
    };
    if found == expected {
        return Ok(());
    }
    let id_type = type_name(expected);
    let cast = if array { format!("{id_type}[]") } else { id_type.clone() };
    Err(ExternalIdError::IdType {
        graph: graph.to_owned(),
        id_type,
        argument,
        found: type_name(oid),
        cast,
    })
}
