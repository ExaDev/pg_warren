//! External ids in and out of SQL functions: polymorphic (`anyelement`,
//! `anyarray`) arguments checked against the graph's id type, mapped to
//! dense ids through the version's id map, and results mapped back
//! (DESIGN.md §5.2, §9.3, §14 item 1). Used by every feature that takes or
//! returns node ids (`traversal_sql`, `compute_sql`).
//!
//! An external id the version does not know, or knows only as tombstoned,
//! is not a node of that version: it is skipped as a start or filter
//! member, and a lookup of it finds nothing. NULL ids are skipped too.

mod error;

pub(crate) use error::ExternalIdError;

use pgrx::prelude::*;
use pgrx::{AnyArray, AnyElement, FromDatum, IntoDatum};
use warren_core::graph::{DenseId, IdMap};

use crate::source::{type_name, IdDatum};

/// The external id `value` holds, checked to be of the graph's id type.
pub(crate) fn element<E: IdDatum>(
    graph: &str,
    argument: &'static str,
    value: AnyElement,
) -> Result<E, ExternalIdError> {
    check_type::<E>(graph, argument, value.oid(), false)?;
    // SAFETY: the datum's type was checked above to be E's SQL type.
    let sql = unsafe { E::Sql::from_polymorphic_datum(value.datum(), false, value.oid()) };
    sql.map(E::from_sql).ok_or(ExternalIdError::Null(argument))
}

/// [`element`], but an `int2` or `int4` value is also accepted for a
/// wider integer id type: for lookups whose result type does not depend
/// on the argument's, so `warren.degree('g', 42)` works on a `bigint`
/// graph.
pub(crate) fn element_widened<E: IdDatum>(
    graph: &str,
    argument: &'static str,
    value: AnyElement,
) -> Result<E, ExternalIdError> {
    // SAFETY: each read below is of the datum's own type, `value.oid()`.
    let narrow = unsafe {
        match value.oid() {
            pg_sys::INT2OID => {
                i16::from_polymorphic_datum(value.datum(), false, value.oid()).map(i64::from)
            }
            pg_sys::INT4OID if value.oid() != E::Sql::type_oid() => {
                i32::from_polymorphic_datum(value.datum(), false, value.oid()).map(i64::from)
            }
            _ => None,
        }
    };
    match narrow.and_then(E::from_integer) {
        Some(id) => Ok(id),
        None => element(graph, argument, value),
    }
}

/// The non-NULL external ids `array` holds, checked to be an array of the
/// graph's id type.
pub(crate) fn elements<E: IdDatum>(
    graph: &str,
    argument: &'static str,
    array: AnyArray,
) -> Result<Vec<E>, ExternalIdError> {
    check_type::<E>(graph, argument, array.oid(), true)?;
    Ok((&array)
        .into_iter()
        .flatten()
        // SAFETY: the element type was checked above to be E's SQL type.
        .filter_map(|value| unsafe {
            E::Sql::from_polymorphic_datum(value.datum(), false, value.oid())
        })
        .map(E::from_sql)
        .collect())
}

/// The dense ids of the live nodes among `ids`, in order.
pub(crate) fn dense_ids<E: IdDatum>(id_map: &IdMap<E>, ids: &[E]) -> Vec<DenseId> {
    ids.iter().filter_map(|id| id_map.dense_id(id)).collect()
}

/// The external id of `dense` as a SQL value of the graph's id type.
pub(crate) fn external<E: IdDatum>(id_map: &IdMap<E>, dense: DenseId) -> Option<AnyElement> {
    let id = id_map.external_id(dense)?;
    let datum = id.to_sql().into_datum()?;
    // SAFETY: the datum was just made from E's SQL type, whose oid is
    // E::Sql::type_oid().
    unsafe { AnyElement::from_polymorphic_datum(datum, false, E::Sql::type_oid()) }
}

/// The external ids of `path` as a SQL array of the graph's id type.
pub(crate) fn external_array<E: IdDatum>(id_map: &IdMap<E>, path: &[DenseId]) -> Option<AnyArray> {
    let ids: Vec<E::Sql> =
        path.iter().filter_map(|&dense| id_map.external_id(dense)).map(E::to_sql).collect();
    let datum = ids.into_datum()?;
    // SAFETY: get_array_type only reads the type cache.
    let array_type = unsafe { pg_sys::get_array_type(E::Sql::type_oid()) };
    // SAFETY: the datum was just made as an array of E's SQL type.
    unsafe { AnyArray::from_polymorphic_datum(datum, false, array_type) }
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
    let expected = E::Sql::type_oid();
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
