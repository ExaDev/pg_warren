//! The types a graph's node ids may have (`graphs.id_type`, DESIGN.md §5.1,
//! §5.2), and the external ids of `warren-core` as Postgres values.

use pgrx::{pg_sys, FromDatum, IntoDatum, Uuid};
use warren_core::graph::ExternalId;

/// A graph's id type: the type of its node query's one column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdType {
    /// `integer`, as `i32` external ids.
    Int4,
    /// `bigint`, as `i64` external ids.
    Int8,
    /// `uuid`, as `[u8; 16]` external ids.
    Uuid,
}

impl IdType {
    /// The id type with this type oid, if it is one.
    pub(crate) fn from_oid(oid: pg_sys::Oid) -> Option<Self> {
        match oid {
            pg_sys::INT4OID => Some(Self::Int4),
            pg_sys::INT8OID => Some(Self::Int8),
            pg_sys::UUIDOID => Some(Self::Uuid),
            _ => None,
        }
    }

    /// The type's oid.
    pub(crate) fn oid(self) -> pg_sys::Oid {
        match self {
            Self::Int4 => pg_sys::INT4OID,
            Self::Int8 => pg_sys::INT8OID,
            Self::Uuid => pg_sys::UUIDOID,
        }
    }

    /// The type's SQL name, as `format_type` prints it.
    pub(crate) fn sql_name(self) -> &'static str {
        match self {
            Self::Int4 => "integer",
            Self::Int8 => "bigint",
            Self::Uuid => "uuid",
        }
    }
}

/// An external id type with its Postgres value type: what the adapter
/// reads from source rows and SQL arguments and returns to SQL.
pub(crate) trait IdDatum: ExternalId + 'static {
    /// The Rust type pgrx reads and writes the SQL value as.
    type Sql: FromDatum + IntoDatum;

    /// The external id of a SQL value.
    fn from_sql(value: Self::Sql) -> Self;
}

impl IdDatum for i32 {
    type Sql = i32;

    fn from_sql(value: i32) -> Self {
        value
    }
}

impl IdDatum for i64 {
    type Sql = i64;

    fn from_sql(value: i64) -> Self {
        value
    }
}

impl IdDatum for [u8; 16] {
    type Sql = Uuid;

    fn from_sql(value: Uuid) -> Self {
        *value.as_bytes()
    }
}

/// Evaluates `$body` with `$E` the external id type of `$id_type`
/// (`i32`, `i64` or `[u8; 16]`): how the adapter dispatches a call on a
/// graph's id type to code generic over [`IdDatum`].
macro_rules! with_id_type {
    ($id_type:expr, $E:ident => $body:expr) => {
        match $id_type {
            $crate::source::IdType::Int4 => {
                type $E = i32;
                $body
            }
            $crate::source::IdType::Int8 => {
                type $E = i64;
                $body
            }
            $crate::source::IdType::Uuid => {
                type $E = [u8; 16];
                $body
            }
        }
    };
}
pub(crate) use with_id_type;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_types_round_trip_through_their_oids() {
        for id_type in [IdType::Int4, IdType::Int8, IdType::Uuid] {
            assert_eq!(IdType::from_oid(id_type.oid()), Some(id_type));
        }
        assert_eq!(IdType::from_oid(pg_sys::TEXTOID), None);
    }

    #[test]
    fn external_ids_are_read_from_their_sql_values() {
        assert_eq!(i32::from_sql(7), 7);
        assert_eq!(i64::from_sql(-7), -7);
        assert_eq!(<[u8; 16]>::from_sql(Uuid::from_bytes([9; 16])), [9; 16]);
    }

    #[test]
    fn dispatch_picks_the_external_id_type() {
        fn width(id_type: IdType) -> usize {
            with_id_type!(id_type, E => <E as ExternalId>::WIDTH)
        }
        assert_eq!(width(IdType::Int4), 4);
        assert_eq!(width(IdType::Int8), 8);
        assert_eq!(width(IdType::Uuid), 16);
    }
}
