//! External ids spelled as text in traversal arguments (`warren.step`
//! filters); the rest of id handling is in `external_ids`.

use std::ffi::CString;

use pgrx::prelude::*;
use pgrx::FromDatum;

use super::TraversalSqlError;
use crate::source::IdDatum;

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
