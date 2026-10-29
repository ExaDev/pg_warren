//! Why no graph queries could be generated from a list of tables' keys.

use pgrx::PgSqlErrorCode;
use thiserror::Error;

use crate::access::SqlError;

/// Why the listed tables give no node and edge queries.
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum KeysError {
    /// The list is empty.
    #[error("tables must list at least one table")]
    NoTables,
    /// The list holds a NULL.
    #[error("tables must not hold NULL")]
    NullTable,
    /// An oid in the list names no relation.
    #[error("relation with OID {0} does not exist")]
    UndefinedTable(u32),
    /// A listed table has no primary key, so its rows have no ids.
    #[error(
        "table {0} has no primary key: its rows need one to be nodes; add one, or write the \
         graph's node and edge queries with warren.create_graph"
    )]
    NoPrimaryKey(String),
}

impl SqlError for KeysError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::NoTables => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            Self::NullTable => PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED,
            Self::UndefinedTable(_) => PgSqlErrorCode::ERRCODE_UNDEFINED_TABLE,
            Self::NoPrimaryKey(_) => PgSqlErrorCode::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE,
        }
    }
}
