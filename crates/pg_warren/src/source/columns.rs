//! The result columns a graph's node query and edge query must have
//! (DESIGN.md §5.1, §9.3).

use pgrx::{pg_sys, PgSqlErrorCode};
use thiserror::Error;

use super::IdType;
use crate::access::SqlError;

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
    /// The query is not one SELECT (or VALUES, or TABLE).
    #[error("{query} must be a single SELECT (or VALUES, or TABLE)")]
    NotASelect {
        /// The query's name.
        query: &'static str,
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
            Self::NulByte { .. } => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            Self::NotASelect { .. } => PgSqlErrorCode::ERRCODE_SYNTAX_ERROR,
        }
    }
}

/// The one column a node query must return: its node ids, whose type is the
/// graph's id type (`integer`, `bigint` or `uuid`). Returns that id type.
pub(crate) fn check_node_columns(columns: &[ColumnType]) -> Result<IdType, SourceError> {
    let [id] = columns else {
        return Err(SourceError::ColumnCount {
            query: "node_query",
            expected: "1",
            found: columns.len(),
        });
    };
    IdType::from_oid(id.oid)
        .ok_or_else(|| mistyped("node_query", 1, "node id", "integer, bigint or uuid", id))
}

/// Checks that a node query returns the node ids of a graph whose id type
/// is `id_type`: its sources may have changed since the graph was created.
pub(crate) fn require_node_id_type(
    columns: &[ColumnType],
    id_type: IdType,
) -> Result<(), SourceError> {
    let found = check_node_columns(columns)?;
    match columns {
        [id] if found != id_type => {
            Err(mistyped("node_query", 1, "node id", id_type.sql_name(), id))
        }
        _ => Ok(()),
    }
}

/// The columns an edge query must return: source and target ids of the
/// graph's id type, then a `text` relation if the graph has relations, then
/// a `float8` weight if it is weighted.
pub(crate) fn check_edge_columns(
    columns: &[ColumnType],
    id_type: IdType,
    has_relations: bool,
    weighted: bool,
) -> Result<(), SourceError> {
    let id = (id_type.oid(), id_type.sql_name());
    let mut expected = vec![("source id", id), ("target id", id)];
    if has_relations {
        expected.push(("relation", (pg_sys::TEXTOID, "text")));
    }
    if weighted {
        expected.push(("weight", (pg_sys::FLOAT8OID, "double precision")));
    }
    if columns.len() != expected.len() {
        return Err(SourceError::ColumnCount {
            query: "edge_query",
            expected: ["2", "3", "4"][expected.len() - 2],
            found: columns.len(),
        });
    }
    for (number, (column, (role, (oid, name)))) in (1..).zip(columns.iter().zip(expected)) {
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
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use super::*;

    fn column(oid: pg_sys::Oid, name: &str) -> ColumnType {
        ColumnType { oid, name: name.to_owned() }
    }

    fn int8() -> ColumnType {
        column(pg_sys::INT8OID, "bigint")
    }

    fn int4() -> ColumnType {
        column(pg_sys::INT4OID, "integer")
    }

    fn uuid() -> ColumnType {
        column(pg_sys::UUIDOID, "uuid")
    }

    #[test]
    fn a_node_query_returns_one_column_of_an_id_type() {
        assert_eq!(check_node_columns(&[int8()]), Ok(IdType::Int8));
        assert_eq!(check_node_columns(&[int4()]), Ok(IdType::Int4));
        assert_eq!(check_node_columns(&[uuid()]), Ok(IdType::Uuid));
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
                expected: "integer, bigint or uuid",
                found: "text".to_owned(),
            })
        );
    }

    #[test]
    fn a_node_query_must_keep_its_graphs_id_type() {
        assert_eq!(require_node_id_type(&[int8()], IdType::Int8), Ok(()));
        assert_eq!(
            require_node_id_type(&[int4()], IdType::Int8),
            Err(SourceError::ColumnType {
                query: "node_query",
                column: 1,
                role: "node id",
                expected: "bigint",
                found: "integer".to_owned(),
            })
        );
        assert_eq!(
            require_node_id_type(&[], IdType::Int8),
            Err(SourceError::ColumnCount { query: "node_query", expected: "1", found: 0 })
        );
    }

    #[test]
    fn an_edge_query_has_a_column_per_flag() {
        let text = column(pg_sys::TEXTOID, "text");
        let float8 = column(pg_sys::FLOAT8OID, "double precision");
        let id = IdType::Int8;
        assert_eq!(check_edge_columns(&[int8(), int8()], id, false, false), Ok(()));
        assert_eq!(check_edge_columns(&[int8(), int8(), text.clone()], id, true, false), Ok(()));
        assert_eq!(check_edge_columns(&[int8(), int8(), float8.clone()], id, false, true), Ok(()));
        assert_eq!(
            check_edge_columns(&[int8(), int8(), text.clone(), float8], id, true, true),
            Ok(())
        );
        assert_eq!(
            check_edge_columns(&[int8(), int8(), text.clone()], id, false, false),
            Err(SourceError::ColumnCount { query: "edge_query", expected: "2", found: 3 })
        );
        assert_eq!(
            check_edge_columns(&[int8(), int8(), text], id, false, true),
            Err(SourceError::ColumnType {
                query: "edge_query",
                column: 3,
                role: "weight",
                expected: "double precision",
                found: "text".to_owned(),
            })
        );
    }

    #[test]
    fn edge_endpoints_have_the_graphs_id_type() {
        assert_eq!(check_edge_columns(&[int4(), int4()], IdType::Int4, false, false), Ok(()));
        assert_eq!(check_edge_columns(&[uuid(), uuid()], IdType::Uuid, false, false), Ok(()));
        assert_eq!(
            check_edge_columns(&[int8(), int4()], IdType::Int8, false, false),
            Err(SourceError::ColumnType {
                query: "edge_query",
                column: 2,
                role: "target id",
                expected: "bigint",
                found: "integer".to_owned(),
            })
        );
        assert_eq!(
            check_edge_columns(&[int8(), int8()], IdType::Uuid, false, false),
            Err(SourceError::ColumnType {
                query: "edge_query",
                column: 1,
                role: "source id",
                expected: "uuid",
                found: "bigint".to_owned(),
            })
        );
    }
}
