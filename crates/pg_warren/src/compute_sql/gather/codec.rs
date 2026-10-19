//! How `gather` reads and writes messages of a SQL type, and the type of
//! each combiner's result (DESIGN.md §6.4).
//!
//! `smallint`, `integer`, `bigint`, `real`, `double precision` and
//! `boolean` messages take the fixed-width path (integers widened to
//! `bigint`); `concat` reads `text` (also `varchar`, `char`) or `bytea`
//! payloads; every other message type travels as its binary send form and
//! is rebuilt with its receive function.

use pgrx::pg_sys::{
    self, BOOLOID, BPCHAROID, BYTEAOID, FLOAT4OID, FLOAT8OID, INT2OID, INT4OID, INT8OID, TEXTOID,
    VARCHAROID,
};
use thiserror::Error;
use warren_core::compute::{Combiner, Dtype, MessageKind};

use crate::source::type_name;

/// A combiner that does not accept the messages' type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum TypeError {
    /// `combine := <combiner>` over messages of type `message`.
    #[error("combine := {combiner} does not accept {} messages", type_name(*.message))]
    Message {
        /// The combiner.
        combiner: Combiner,
        /// The messages' SQL type.
        message: pg_sys::Oid,
    },
}

/// How messages of one SQL type are read and written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Codec {
    /// As vector elements of `dtype`, read as SQL type `read`; `sql` is the
    /// messages' own type.
    Element {
        /// The element type.
        dtype: Dtype,
        /// The messages' type.
        sql: pg_sys::Oid,
        /// The type they are read as.
        read: pg_sys::Oid,
    },
    /// As the bytes of a `text` or `bytea` value (`sql`), for `concat`.
    Payload {
        /// `text` or `bytea`.
        sql: pg_sys::Oid,
    },
    /// As the binary send form of type `sql`.
    Binary {
        /// The messages' type.
        sql: pg_sys::Oid,
    },
}

/// The type of a combiner's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResultType {
    /// That type.
    Of(pg_sys::Oid),
    /// An array of that type, as `array_agg` makes of it (for an array
    /// type, the same array type, with one more dimension).
    ArrayOf(pg_sys::Oid),
}

/// The element type messages of SQL type `sql` take the fixed-width path
/// as, and the type they are read as.
fn element(sql: pg_sys::Oid) -> Option<(Dtype, pg_sys::Oid)> {
    match sql {
        INT2OID | INT4OID | INT8OID => Some((Dtype::I64, INT8OID)),
        FLOAT4OID => Some((Dtype::F32, FLOAT4OID)),
        FLOAT8OID => Some((Dtype::F64, FLOAT8OID)),
        BOOLOID => Some((Dtype::Bool, BOOLOID)),
        _ => None,
    }
}

impl Codec {
    /// How `combiner` reads messages of SQL type `message` (a base type).
    pub(super) fn choose(message: pg_sys::Oid, combiner: Combiner) -> Result<Self, TypeError> {
        let refused = TypeError::Message { combiner, message };
        let codec = match (combiner, element(message)) {
            (Combiner::Concat, _) => match message {
                TEXTOID | VARCHAROID | BPCHAROID => Self::Payload { sql: TEXTOID },
                BYTEAOID => Self::Payload { sql: BYTEAOID },
                _ => return Err(refused),
            },
            (_, Some((dtype, read))) => Self::Element { dtype, sql: message, read },
            (Combiner::Collect | Combiner::Count | Combiner::First, None) => {
                Self::Binary { sql: message }
            }
            (_, None) => return Err(refused),
        };
        combiner.check(codec.kind()).map_err(|_| refused)?;
        Ok(codec)
    }

    /// The message kind the kernel sees.
    pub(super) fn kind(self) -> MessageKind {
        match self {
            Self::Element { dtype, .. } => MessageKind::Element(dtype),
            Self::Payload { .. } | Self::Binary { .. } => MessageKind::Bytes,
        }
    }

    /// The SQL type messages are read as.
    pub(super) fn read_type(self) -> pg_sys::Oid {
        match self {
            Self::Element { read, .. } => read,
            Self::Payload { sql } | Self::Binary { sql } => sql,
        }
    }

    /// The SQL type of the messages a result holds.
    pub(super) fn message_type(self) -> pg_sys::Oid {
        match self {
            Self::Element { sql, .. } | Self::Payload { sql } | Self::Binary { sql } => sql,
        }
    }

    /// The type of `combiner`'s result.
    pub(super) fn result(self, combiner: Combiner) -> ResultType {
        let sql = self.message_type();
        ResultType::Of(match combiner {
            Combiner::Count => INT8OID,
            Combiner::Collect => return ResultType::ArrayOf(sql),
            Combiner::Sum if matches!(sql, INT2OID | INT4OID) => INT8OID,
            Combiner::Sum | Combiner::Min | Combiner::Max | Combiner::First => sql,
            Combiner::BoolOr | Combiner::BoolAnd => BOOLOID,
            Combiner::Concat => sql,
        })
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use super::*;
    use pgrx::pg_sys::{
        BOOLOID, BPCHAROID, BYTEAOID, FLOAT4OID, FLOAT8OID, INT2OID, INT4OID, INT8OID, JSONBOID,
        NUMERICOID, TEXTOID, VARCHAROID,
    };
    use warren_core::compute::{Combiner, Dtype};

    fn choose(message: pg_sys::Oid, combiner: Combiner) -> Option<Codec> {
        Codec::choose(message, combiner).ok()
    }

    #[test]
    fn numbers_and_booleans_take_the_fixed_width_path() {
        let element = |dtype, sql, read| Some(Codec::Element { dtype, sql, read });
        for sql in [INT2OID, INT4OID, INT8OID] {
            assert_eq!(choose(sql, Combiner::Sum), element(Dtype::I64, sql, INT8OID));
        }
        assert_eq!(choose(FLOAT4OID, Combiner::Max), element(Dtype::F32, FLOAT4OID, FLOAT4OID));
        assert_eq!(choose(FLOAT8OID, Combiner::Collect), element(Dtype::F64, FLOAT8OID, FLOAT8OID));
        assert_eq!(choose(BOOLOID, Combiner::BoolAnd), element(Dtype::Bool, BOOLOID, BOOLOID));
    }

    #[test]
    fn concat_reads_text_or_bytea_payloads() {
        for sql in [TEXTOID, VARCHAROID, BPCHAROID] {
            assert_eq!(choose(sql, Combiner::Concat), Some(Codec::Payload { sql: TEXTOID }));
        }
        assert_eq!(choose(BYTEAOID, Combiner::Concat), Some(Codec::Payload { sql: BYTEAOID }));
    }

    #[test]
    fn other_types_travel_in_their_binary_form() {
        for combiner in [Combiner::Collect, Combiner::First, Combiner::Count] {
            assert_eq!(choose(JSONBOID, combiner), Some(Codec::Binary { sql: JSONBOID }));
            assert_eq!(choose(TEXTOID, combiner), Some(Codec::Binary { sql: TEXTOID }));
        }
    }

    #[test]
    fn combiners_refuse_types_sql_would() {
        let refused = [
            (NUMERICOID, Combiner::Sum),
            (TEXTOID, Combiner::Min),
            (BOOLOID, Combiner::Sum),
            (INT8OID, Combiner::BoolOr),
            (INT8OID, Combiner::Concat),
            (JSONBOID, Combiner::Concat),
        ];
        for (message, combiner) in refused {
            assert!(
                matches!(
                    Codec::choose(message, combiner),
                    Err(TypeError::Message { combiner: c, message: m }) if c == combiner && m == message
                ),
                "{combiner} over {message:?}"
            );
        }
    }

    #[test]
    fn each_combiner_has_its_result_type() {
        let int4 = Codec::choose(INT4OID, Combiner::Sum).unwrap();
        assert_eq!(int4.result(Combiner::Sum), ResultType::Of(INT8OID));
        assert_eq!(int4.result(Combiner::Min), ResultType::Of(INT4OID));
        assert_eq!(int4.result(Combiner::Count), ResultType::Of(INT8OID));
        assert_eq!(int4.result(Combiner::Collect), ResultType::ArrayOf(INT4OID));
        let real = Codec::choose(FLOAT4OID, Combiner::Sum).unwrap();
        assert_eq!(real.result(Combiner::Sum), ResultType::Of(FLOAT4OID));
        let flag = Codec::choose(BOOLOID, Combiner::BoolOr).unwrap();
        assert_eq!(flag.result(Combiner::BoolOr), ResultType::Of(BOOLOID));
        let text = Codec::choose(VARCHAROID, Combiner::Concat).unwrap();
        assert_eq!(text.result(Combiner::Concat), ResultType::Of(TEXTOID));
        let json = Codec::choose(JSONBOID, Combiner::First).unwrap();
        assert_eq!(json.result(Combiner::First), ResultType::Of(JSONBOID));
        assert_eq!(json.result(Combiner::Collect), ResultType::ArrayOf(JSONBOID));
    }
}
