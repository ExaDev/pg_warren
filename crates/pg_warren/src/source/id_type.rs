//! The types a graph's node ids may have (`graphs.id_type`, DESIGN.md §5.1,
//! §5.2, §5.7), and the external ids of `warren-core` as Postgres values.

use std::borrow::Borrow;
use std::fmt::Debug;

use pgrx::{pg_sys, FromDatum, IntoDatum, Uuid};
use warren_core::graph::{
    BuildBuffer, BuildMemory, ExternalId, FixedKeys, GraphError, IdMap, IndexedKeys, TextIds,
    TextIndex, TextKeys,
};

/// A graph's id type: the type of its node query's one column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdType {
    /// `integer`, as `i32` external ids.
    Int4,
    /// `bigint`, as `i64` external ids.
    Int8,
    /// `uuid`, as `[u8; 16]` external ids.
    Uuid,
    /// `text` (or `varchar`, read as `text`), as the bytes of each id in
    /// the database encoding.
    Text,
}

impl IdType {
    /// The id type with this type oid, if it is one: `varchar` is `text`.
    pub(crate) fn from_oid(oid: pg_sys::Oid) -> Option<Self> {
        match oid {
            pg_sys::INT4OID => Some(Self::Int4),
            pg_sys::INT8OID => Some(Self::Int8),
            pg_sys::UUIDOID => Some(Self::Uuid),
            pg_sys::TEXTOID | pg_sys::VARCHAROID => Some(Self::Text),
            _ => None,
        }
    }

    /// The type's oid.
    pub(crate) fn oid(self) -> pg_sys::Oid {
        match self {
            Self::Int4 => pg_sys::INT4OID,
            Self::Int8 => pg_sys::INT8OID,
            Self::Uuid => pg_sys::UUIDOID,
            Self::Text => pg_sys::TEXTOID,
        }
    }

    /// The type's SQL name, as `format_type` prints it.
    pub(crate) fn sql_name(self) -> &'static str {
        match self {
            Self::Int4 => "integer",
            Self::Int8 => "bigint",
            Self::Uuid => "uuid",
            Self::Text => "text",
        }
    }

    /// Whether a column of type `oid` holds ids of this type: its own
    /// type, or `varchar` for `text`, which has the same representation.
    pub(crate) fn reads(self, oid: pg_sys::Oid) -> bool {
        Self::from_oid(oid) == Some(self)
    }
}

/// The key storage of an id type with its Postgres values: how the adapter
/// reads ids from source rows and SQL arguments and returns them to SQL.
/// Implemented by [`FixedKeys`] of each [`InPlaceId`] and by [`TextKeys`].
pub(crate) trait IdDatum: IndexedKeys + 'static {
    /// The id type.
    const ID_TYPE: IdType;
    /// An external id held by value, as an argument holds it.
    type Owned: Borrow<Self::Key> + Clone + Debug + Send + Sync + 'static;
    /// An external id read in place from a datum that lives for `'t`.
    type InPlace<'t>: Borrow<Self::Key>;

    /// The id a datum holds, read in place (a text id is detoasted, if it
    /// must be, into the current memory context, else not copied), or
    /// `None` for a NULL datum.
    ///
    /// # Safety
    ///
    /// `datum` is NULL or a datum of a type [`IdType::reads`] accepts for
    /// [`Self::ID_TYPE`], which lives for `'t`.
    unsafe fn in_place<'t>(datum: pg_sys::Datum, is_null: bool) -> Option<Self::InPlace<'t>>;

    /// The id `key`, by value.
    fn owned(key: &Self::Key) -> Self::Owned;

    /// A datum of [`Self::ID_TYPE`] holding `key`. A text id is checked to
    /// be valid in the database encoding first (DESIGN.md §5.7), so no
    /// invalid text reaches SQL: a version's bytes are not checked when it
    /// is loaded.
    fn datum(key: &Self::Key) -> Option<pg_sys::Datum>;

    /// The external id an integer of a narrower SQL type widens to, if
    /// this is an integer id type it fits.
    fn from_integer(value: i64) -> Option<Self::Owned>;

    /// No node ids, to take a node query's, charged to `memory`.
    fn node_ids(memory: &BuildMemory) -> Self::Ids;

    /// Appends `key` to `ids`.
    fn push_node_id(ids: &mut Self::Ids, key: &Self::Key) -> Result<(), GraphError>;

    /// The bytes of the hash index a call that maps many ids builds over
    /// `id_map` (DESIGN.md §5.7), or `None` for an id type that maps them by
    /// binary search alone.
    fn lookup_index_bytes(id_map: &IdMap<Self>) -> Option<u64>;
}

/// A fixed-width external id type with its Postgres value type.
///
/// # Safety
///
/// A loaded version's id map is read in place from its region (DESIGN.md
/// §5.6), so every pattern of `size_of::<Self>()` initialized bytes must
/// be a valid id, the type must have no padding, and its alignment must be
/// at most 8. Its segments are copied there unchanged on a little-endian
/// target, so `Self::WIDTH` must be `size_of::<Self>()` and, on such a
/// target, an id's encoding (`write_le`) must be its bytes in memory.
pub(crate) unsafe trait InPlaceId: ExternalId + Send + Sync + 'static {
    /// The id type.
    const ID_TYPE: IdType;
    /// The Rust type pgrx reads and writes the SQL value as.
    type Sql: FromDatum + IntoDatum;

    /// The external id of a SQL value.
    fn from_sql(value: Self::Sql) -> Self;

    /// The SQL value of an external id.
    fn to_sql(self) -> Self::Sql;

    /// The external id an integer of a narrower SQL type widens to, if
    /// this is an integer id type it fits.
    fn from_integer(value: i64) -> Option<Self>;
}

// SAFETY: an integer: any bits are a value, no padding, alignment 4;
// encoded little-endian in its size.
unsafe impl InPlaceId for i32 {
    const ID_TYPE: IdType = IdType::Int4;
    type Sql = i32;

    fn from_sql(value: i32) -> Self {
        value
    }

    fn to_sql(self) -> i32 {
        self
    }

    fn from_integer(value: i64) -> Option<Self> {
        i32::try_from(value).ok()
    }
}

// SAFETY: an integer: any bits are a value, no padding, alignment 8;
// encoded little-endian in its size.
unsafe impl InPlaceId for i64 {
    const ID_TYPE: IdType = IdType::Int8;
    type Sql = i64;

    fn from_sql(value: i64) -> Self {
        value
    }

    fn to_sql(self) -> i64 {
        self
    }

    fn from_integer(value: i64) -> Option<Self> {
        Some(value)
    }
}

// SAFETY: bytes: any bits are a value, no padding, alignment 1;
// encoded as themselves.
unsafe impl InPlaceId for [u8; 16] {
    const ID_TYPE: IdType = IdType::Uuid;
    type Sql = Uuid;

    fn from_sql(value: Uuid) -> Self {
        *value.as_bytes()
    }

    fn to_sql(self) -> Uuid {
        Uuid::from_bytes(self)
    }

    fn from_integer(_: i64) -> Option<Self> {
        None
    }
}

impl<E: InPlaceId> IdDatum for FixedKeys<E> {
    const ID_TYPE: IdType = E::ID_TYPE;
    type Owned = E;
    type InPlace<'t> = E;

    unsafe fn in_place<'t>(datum: pg_sys::Datum, is_null: bool) -> Option<Self::InPlace<'t>> {
        // SAFETY: the caller guarantees a NULL datum or one of E's SQL type.
        let sql = unsafe { E::Sql::from_polymorphic_datum(datum, is_null, E::ID_TYPE.oid()) };
        sql.map(E::from_sql)
    }

    fn owned(key: &E) -> E {
        *key
    }

    fn datum(key: &E) -> Option<pg_sys::Datum> {
        key.to_sql().into_datum()
    }

    fn from_integer(value: i64) -> Option<E> {
        E::from_integer(value)
    }

    fn node_ids(memory: &BuildMemory) -> BuildBuffer<E> {
        BuildBuffer::new(memory)
    }

    fn push_node_id(ids: &mut BuildBuffer<E>, key: &E) -> Result<(), GraphError> {
        ids.push(*key)
    }

    fn lookup_index_bytes(_: &IdMap<Self>) -> Option<u64> {
        None
    }
}

impl IdDatum for TextKeys {
    const ID_TYPE: IdType = IdType::Text;
    type Owned = Vec<u8>;
    type InPlace<'t> = &'t [u8];

    unsafe fn in_place<'t>(datum: pg_sys::Datum, is_null: bool) -> Option<Self::InPlace<'t>> {
        // SAFETY: the caller guarantees a NULL datum or a `text` or
        // `varchar` one living for 't; both are varlenas, read as their
        // bytes.
        unsafe { <&[u8]>::from_polymorphic_datum(datum, is_null, pg_sys::TEXTOID) }
    }

    fn owned(key: &[u8]) -> Vec<u8> {
        key.to_vec()
    }

    fn datum(key: &[u8]) -> Option<pg_sys::Datum> {
        let Ok(len) = i32::try_from(key.len()) else {
            pgrx::ereport!(
                ERROR,
                pgrx::PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
                format!("a text id of {} bytes is too long to return", key.len())
            );
        };
        let start = key.as_ptr().cast::<std::ffi::c_char>();
        // SAFETY: `start` points to `len` readable bytes. Both functions
        // are guarded and raise an ERROR rather than return a bad value:
        // pg_verifymbstr, with `noError` false, raises 22021 for bytes
        // invalid in the database encoding, and the copy raises 54000 for
        // a text past the varlena limit.
        unsafe {
            pg_sys::pg_verifymbstr(start, len, false);
            Some(pg_sys::Datum::from(pg_sys::cstring_to_text_with_len(start, len)))
        }
    }

    fn from_integer(_: i64) -> Option<Vec<u8>> {
        None
    }

    fn node_ids(memory: &BuildMemory) -> TextIds {
        TextIds::new(memory)
    }

    fn push_node_id(ids: &mut TextIds, key: &[u8]) -> Result<(), GraphError> {
        ids.push(key)
    }

    fn lookup_index_bytes(id_map: &IdMap<Self>) -> Option<u64> {
        Some(TextIndex::bytes_for(id_map))
    }
}

/// Evaluates `$body` with `$E` the external id type of `$id_type`
/// (`FixedKeys` of `i32`, `i64` or `[u8; 16]`, or `TextKeys`): how the adapter dispatches a call on a
/// graph's id type to code generic over [`IdDatum`].
macro_rules! with_id_type {
    ($id_type:expr, $E:ident => $body:expr) => {
        match $id_type {
            $crate::source::IdType::Int4 => {
                type $E = ::warren_core::graph::FixedKeys<i32>;
                $body
            }
            $crate::source::IdType::Int8 => {
                type $E = ::warren_core::graph::FixedKeys<i64>;
                $body
            }
            $crate::source::IdType::Uuid => {
                type $E = ::warren_core::graph::FixedKeys<[u8; 16]>;
                $body
            }
            $crate::source::IdType::Text => {
                type $E = ::warren_core::graph::TextKeys;
                $body
            }
        }
    };
}
pub(crate) use with_id_type;

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use super::*;

    #[test]
    fn an_external_id_is_encoded_as_its_bytes_in_memory() {
        fn check<E: InPlaceId>(id: E, in_memory: &[u8]) {
            assert_eq!(E::WIDTH, std::mem::size_of::<E>());
            let mut encoded = Vec::new();
            id.write_le(&mut encoded);
            if cfg!(target_endian = "little") {
                assert_eq!(encoded, in_memory, "{id:?}");
            }
            assert_eq!(E::read_le(&encoded), id);
        }
        for id in [i32::MIN, -7, 0, 0x0102_0304, i32::MAX] {
            check(id, &id.to_ne_bytes());
        }
        for id in [i64::MIN, -7, 0, 0x0102_0304_0506_0708, i64::MAX] {
            check(id, &id.to_ne_bytes());
        }
        let uuid: [u8; 16] = std::array::from_fn(|i| i as u8 * 17);
        check(uuid, &uuid);
    }

    #[test]
    fn id_types_round_trip_through_their_oids() {
        for id_type in [IdType::Int4, IdType::Int8, IdType::Uuid, IdType::Text] {
            assert_eq!(IdType::from_oid(id_type.oid()), Some(id_type));
            assert!(id_type.reads(id_type.oid()));
        }
        assert_eq!(IdType::from_oid(pg_sys::VARCHAROID), Some(IdType::Text));
        assert!(IdType::Text.reads(pg_sys::VARCHAROID));
        for refused in [pg_sys::BPCHAROID, pg_sys::NAMEOID, pg_sys::INT2OID, pg_sys::BYTEAOID] {
            assert_eq!(IdType::from_oid(refused), None);
        }
    }

    #[test]
    fn external_ids_round_trip_through_their_sql_values() {
        assert_eq!(i32::from_sql(7_i32.to_sql()), 7);
        assert_eq!(i64::from_sql((-7_i64).to_sql()), -7);
        assert_eq!(<[u8; 16]>::from_sql([9; 16].to_sql()), [9; 16]);
    }

    #[test]
    fn narrower_integers_widen_only_to_integer_id_types_they_fit() {
        assert_eq!(i64::from_integer(-5), Some(-5));
        assert_eq!(i32::from_integer(7), Some(7));
        assert_eq!(i32::from_integer(i64::from(i32::MAX) + 1), None);
        assert_eq!(<[u8; 16]>::from_integer(1), None);
        assert_eq!(<TextKeys as IdDatum>::from_integer(1), None);
    }

    #[test]
    fn dispatch_picks_the_external_id_type() {
        fn dispatched(id_type: IdType) -> IdType {
            with_id_type!(id_type, E => <E as IdDatum>::ID_TYPE)
        }
        for id_type in [IdType::Int4, IdType::Int8, IdType::Uuid, IdType::Text] {
            assert_eq!(dispatched(id_type), id_type);
        }
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod in_database {
    use pgrx::prelude::*;
    use warren_core::graph::TextKeys;

    use super::IdDatum;

    /// A text id is checked against the database encoding as it becomes a
    /// datum (DESIGN.md §5.7); the test database is UTF8.
    #[pg_test(error = "invalid byte sequence for encoding \"UTF8\": 0xff")]
    fn a_text_id_invalid_in_the_database_encoding_is_refused() {
        TextKeys::datum(&[b'a', 0xff]);
    }

    #[pg_test]
    fn a_text_id_becomes_a_text_datum() {
        let datum = TextKeys::datum("é".as_bytes());
        // SAFETY: the datum was just made as a text.
        let text = datum.and_then(|datum| unsafe { String::from_datum(datum, false) });
        assert_eq!(text.as_deref(), Some("é"));
    }
}
