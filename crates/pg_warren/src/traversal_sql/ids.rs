//! External ids in and out of traversal functions: polymorphic
//! (`anyelement`, `anyarray`) arguments checked against the graph's id
//! type, mapped to dense ids through the version's id map, and results
//! mapped back (DESIGN.md §5.2, §9.3, §14 item 1).
//!
//! An external id the version does not know, or knows only as tombstoned,
//! is not a node of that version: it is skipped as a start or filter
//! member, and a lookup of it finds nothing. NULL ids are skipped too.

use std::ffi::CString;

use pgrx::prelude::*;
use pgrx::{AnyArray, AnyElement, FromDatum, IntoDatum};
use warren_core::graph::{DenseId, IdMap};

use super::TraversalSqlError;
use crate::source::{type_name, IdDatum};

/// The external id `value` holds, checked to be of the graph's id type.
pub(crate) fn element<E: IdDatum>(
    graph: &str,
    argument: &'static str,
    value: AnyElement,
) -> Result<E, TraversalSqlError> {
    check_type::<E>(graph, argument, value.oid(), false)?;
    // SAFETY: the datum's type was checked above to be E's SQL type.
    let sql = unsafe { E::Sql::from_polymorphic_datum(value.datum(), false, value.oid()) };
    sql.map(E::from_sql).ok_or(TraversalSqlError::NullArgument(argument))
}

/// The non-NULL external ids `array` holds, checked to be an array of the
/// graph's id type.
pub(crate) fn elements<E: IdDatum>(
    graph: &str,
    argument: &'static str,
    array: AnyArray,
) -> Result<Vec<E>, TraversalSqlError> {
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

/// The external id `text` spells, read with the id type's input function
/// (which raises `invalid_text_representation` if it is not one).
pub(crate) fn parse<E: IdDatum>(text: &str) -> Result<E, TraversalSqlError> {
    let text = CString::new(text)
        .map_err(|_| TraversalSqlError::Step("an id contains a NUL byte".to_owned()))?;
    let oid = E::Sql::type_oid();
    let mut input = pg_sys::Oid::INVALID;
    let mut io_param = pg_sys::Oid::INVALID;
    // SAFETY: the out-pointers point to live locals; `text` is
    // NUL-terminated and outlives the call, which raises an ERROR on bad
    // input rather than return.
    let datum = unsafe {
        pg_sys::getTypeInputInfo(oid, &mut input, &mut io_param);
        pg_sys::OidInputFunctionCall(input, text.as_ptr().cast_mut(), io_param, -1)
    };
    // SAFETY: the datum was just made by E's SQL type's input function.
    let sql = unsafe { E::Sql::from_polymorphic_datum(datum, false, oid) };
    sql.map(E::from_sql).ok_or(TraversalSqlError::NullArgument("a filter id"))
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

/// Checks that `oid` is the graph's id type (`array`: an array of it).
pub(crate) fn check_type<E: IdDatum>(
    graph: &str,
    argument: &'static str,
    oid: pg_sys::Oid,
    array: bool,
) -> Result<(), TraversalSqlError> {
    let expected = E::Sql::type_oid();
    let found = match array {
        // SAFETY: get_element_type only reads the type cache.
        true => unsafe { pg_sys::get_element_type(oid) },
        false => oid,
    };
    if found == expected {
        return Ok(());
    }
    let id_type = type_name(expected);
    let cast = if array { format!("{id_type}[]") } else { id_type.clone() };
    Err(TraversalSqlError::IdType {
        graph: graph.to_owned(),
        id_type,
        argument,
        found: type_name(oid),
        cast,
    })
}
