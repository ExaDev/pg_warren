//! A graph's sources: the result columns its node query and edge query must
//! have (DESIGN.md §5.1, §9.3).

use pgrx::{pg_sys, PgSqlErrorCode};
use thiserror::Error;

use crate::extension::SqlError;

/// The type of one result column of a source query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnType {
    /// The type's oid.
    pub(crate) oid: pg_sys::Oid,
    /// The type's SQL name, as `format_type` prints it.
    pub(crate) name: String,
}

/// Why a source query does not fit its graph.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum SourceError {
    /// The query returns the wrong number of columns.
    #[error("{query} must return {expected} columns, not {found}")]
    ColumnCount {
        /// `node_query` or `edge_query`.
        query: &'static str,
        /// The expected number of columns.
        expected: &'static str,
        /// The number of columns the query returns.
        found: usize,
    },
    /// A column has the wrong type.
    #[error("{query} column {column} ({role}) must be of type {expected}, not {found}")]
    ColumnType {
        /// `node_query` or `edge_query`.
        query: &'static str,
        /// The 1-based column number.
        column: usize,
        /// What the column holds.
        role: &'static str,
        /// The expected type.
        expected: &'static str,
        /// The column's type.
        found: String,
    },
    /// The node ids have an id type a later milestone supports.
    #[error("node ids of type {found} are not supported yet; node_query must return bigint")]
    UnsupportedIdType {
        /// The node id type.
        found: String,
    },
    /// The query text contains a NUL byte.
    #[error("{query} contains a NUL byte")]
    NulByte {
        /// `node_query` or `edge_query`.
        query: &'static str,
    },
}

impl SqlError for SourceError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::ColumnCount { .. } | Self::ColumnType { .. } => {
                PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH
            }
            Self::UnsupportedIdType { .. } => PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
            Self::NulByte { .. } => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
        }
    }
}

/// The one column a node query must return: its node ids, whose type is the
/// graph's id type. Returns that type: `int8` (`int4` and `uuid` arrive in M3).
pub(crate) fn check_node_columns(columns: &[ColumnType]) -> Result<pg_sys::Oid, SourceError> {
    let [id] = columns else {
        return Err(SourceError::ColumnCount {
            query: "node_query",
            expected: "1",
            found: columns.len(),
        });
    };
    match id.oid {
        pg_sys::INT8OID => Ok(id.oid),
        pg_sys::INT4OID | pg_sys::UUIDOID => {
            Err(SourceError::UnsupportedIdType { found: id.name.clone() })
        }
        _ => Err(mistyped("node_query", 1, "node id", "bigint", id)),
    }
}

/// The columns an `int8` graph's edge query must return: source and target
/// ids, then a `text` relation if the graph has relations, then a `float8`
/// weight if it is weighted.
pub(crate) fn check_edge_columns(
    columns: &[ColumnType],
    has_relations: bool,
    weighted: bool,
) -> Result<(), SourceError> {
    let mut expected =
        vec![("source id", pg_sys::INT8OID, "bigint"), ("target id", pg_sys::INT8OID, "bigint")];
    if has_relations {
        expected.push(("relation", pg_sys::TEXTOID, "text"));
    }
    if weighted {
        expected.push(("weight", pg_sys::FLOAT8OID, "double precision"));
    }
    if columns.len() != expected.len() {
        return Err(SourceError::ColumnCount {
            query: "edge_query",
            expected: ["2", "3", "4"][expected.len() - 2],
            found: columns.len(),
        });
    }
    for (number, (column, (role, oid, name))) in (1..).zip(columns.iter().zip(expected)) {
        if column.oid != oid {
            return Err(mistyped("edge_query", number, role, name, column));
        }
    }
    Ok(())
}

fn mistyped(
    query: &'static str,
    column: usize,
    role: &'static str,
    expected: &'static str,
    found: &ColumnType,
) -> SourceError {
    SourceError::ColumnType { query, column, role, expected, found: found.name.clone() }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    fn column(oid: pg_sys::Oid, name: &str) -> ColumnType {
        ColumnType { oid, name: name.to_owned() }
    }

    fn int8() -> ColumnType {
        column(pg_sys::INT8OID, "bigint")
    }

    #[test]
    fn a_node_query_returns_one_int8_column() {
        assert_eq!(check_node_columns(&[int8()]), Ok(pg_sys::INT8OID));
        assert_eq!(
            check_node_columns(&[int8(), int8()]),
            Err(SourceError::ColumnCount { query: "node_query", expected: "1", found: 2 })
        );
        assert_eq!(
            check_node_columns(&[column(pg_sys::TEXTOID, "text")]),
            Err(SourceError::ColumnType {
                query: "node_query",
                column: 1,
                role: "node id",
                expected: "bigint",
                found: "text".to_owned(),
            })
        );
    }

    #[test]
    fn int4_and_uuid_node_ids_are_not_supported_yet() {
        for (oid, name) in [(pg_sys::INT4OID, "integer"), (pg_sys::UUIDOID, "uuid")] {
            assert_eq!(
                check_node_columns(&[column(oid, name)]),
                Err(SourceError::UnsupportedIdType { found: name.to_owned() })
            );
        }
    }

    #[test]
    fn an_edge_query_has_a_column_per_flag() {
        let text = column(pg_sys::TEXTOID, "text");
        let float8 = column(pg_sys::FLOAT8OID, "double precision");
        assert_eq!(check_edge_columns(&[int8(), int8()], false, false), Ok(()));
        assert_eq!(check_edge_columns(&[int8(), int8(), text.clone()], true, false), Ok(()));
        assert_eq!(check_edge_columns(&[int8(), int8(), float8.clone()], false, true), Ok(()));
        assert_eq!(check_edge_columns(&[int8(), int8(), text.clone(), float8], true, true), Ok(()));
        assert_eq!(
            check_edge_columns(&[int8(), int8(), text.clone()], false, false),
            Err(SourceError::ColumnCount { query: "edge_query", expected: "2", found: 3 })
        );
        assert_eq!(
            check_edge_columns(&[int8(), int8(), text], false, true),
            Err(SourceError::ColumnType {
                query: "edge_query",
                column: 3,
                role: "weight",
                expected: "double precision",
                found: "text".to_owned(),
            })
        );
        assert_eq!(
            check_edge_columns(&[int8(), column(pg_sys::INT4OID, "integer")], false, false),
            Err(SourceError::ColumnType {
                query: "edge_query",
                column: 2,
                role: "target id",
                expected: "bigint",
                found: "integer".to_owned(),
            })
        );
    }
}
