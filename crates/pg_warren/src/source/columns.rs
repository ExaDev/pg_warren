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
    /// The name of the column's collation if it is nondeterministic
    /// (equality that is not bytewise, DESIGN.md §5.7); `None` for a
    /// deterministic one or a type without collation.
    pub(crate) nondeterministic: Option<String>,
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
    /// An id column of a text-like type whose equality is not bytewise, or
    /// that cannot hold every id.
    #[error(
        "{query} column {column} ({role}) is of type {found}, which node ids cannot have: \
         {why}; cast it to text ({cast})"
    )]
    IdTypeRefused {
        /// `node_query` or `edge_query`.
        query: &'static str,
        /// The 1-based column number.
        column: usize,
        /// What the column holds.
        role: &'static str,
        /// The column's type.
        found: String,
        /// Why the type is refused.
        why: &'static str,
        /// The cast to write.
        cast: &'static str,
    },
    /// A text id column with a nondeterministic collation, whose equality
    /// bytewise ids cannot follow (DESIGN.md §5.7).
    #[error(
        "{query} column {column} ({role}) has the nondeterministic collation \"{collation}\", \
         whose equality node ids cannot follow (they compare as bytes): write the column \
         COLLATE \"C\" for exact ids, or normalise them in the query (for example lower(id))"
    )]
    NondeterministicCollation {
        /// `node_query` or `edge_query`.
        query: &'static str,
        /// The 1-based column number.
        column: usize,
        /// What the column holds.
        role: &'static str,
        /// The collation's name.
        collation: String,
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
            Self::ColumnCount { .. }
            | Self::ColumnType { .. }
            | Self::IdTypeRefused { .. }
            | Self::NondeterministicCollation { .. } => PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH,
            Self::NulByte { .. } => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            Self::NotASelect { .. } => PgSqlErrorCode::ERRCODE_SYNTAX_ERROR,
        }
    }
}

/// The one column a node query must return: its node ids, whose type is the
/// graph's id type (`integer`, `bigint`, `uuid` or `text`, which `varchar`
/// is read as), with a deterministic collation. Returns that id type.
pub(crate) fn check_node_columns(columns: &[ColumnType]) -> Result<IdType, SourceError> {
    let [id] = columns else {
        return Err(SourceError::ColumnCount {
            query: "node_query",
            expected: "1",
            found: columns.len(),
        });
    };
    let at = Column { query: "node_query", number: 1, role: "node id" };
    refuse_text_like(at, id)?;
    let id_type = IdType::from_oid(id.oid)
        .ok_or_else(|| mistyped(at, "integer, bigint, uuid or text", id))?;
    require_deterministic(at, id)?;
    Ok(id_type)
}

/// Where a column is: its query, its 1-based number, and what it holds.
#[derive(Debug, Clone, Copy)]
struct Column {
    query: &'static str,
    number: usize,
    role: &'static str,
}

/// Refuses a text-like type that is not an id type, naming the cast to
/// write: `bpchar`, `name` and `citext` (DESIGN.md §5.7).
fn refuse_text_like(at: Column, column: &ColumnType) -> Result<(), SourceError> {
    let unqualified = column.name.rsplit('.').next().unwrap_or_default();
    let (why, cast) = match column.oid {
        pg_sys::BPCHAROID => ("its equality ignores trailing spaces", "id::text"),
        pg_sys::NAMEOID => ("it is a system identifier type, cut at 63 bytes", "id::text"),
        _ if unqualified == "citext" => {
            ("its equality ignores case", "id::text for exact ids, or lower(id)::text")
        }
        _ => return Ok(()),
    };
    Err(SourceError::IdTypeRefused {
        query: at.query,
        column: at.number,
        role: at.role,
        found: column.name.clone(),
        why,
        cast,
    })
}

/// Refuses an id column with a nondeterministic collation.
fn require_deterministic(at: Column, column: &ColumnType) -> Result<(), SourceError> {
    match &column.nondeterministic {
        None => Ok(()),
        Some(collation) => Err(SourceError::NondeterministicCollation {
            query: at.query,
            column: at.number,
            role: at.role,
            collation: collation.clone(),
        }),
    }
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
            let at = Column { query: "node_query", number: 1, role: "node id" };
            Err(mistyped(at, id_type.sql_name(), id))
        }
        _ => Ok(()),
    }
}

/// The columns an edge query must return: source and target ids of the
/// graph's id type (for `text`, `text` or `varchar`, with a deterministic
/// collation), then a `text` relation if the graph has relations, then a
/// `float8` weight if it is weighted.
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
        let at = Column { query: "edge_query", number, role };
        let endpoint = number <= 2;
        if endpoint {
            refuse_text_like(at, column)?;
        }
        let fits = match endpoint {
            true => id_type.reads(column.oid),
            false => column.oid == oid,
        };
        if !fits {
            return Err(mistyped(at, name, column));
        }
        if endpoint {
            require_deterministic(at, column)?;
        }
    }
    Ok(())
}

fn mistyped(at: Column, expected: &'static str, found: &ColumnType) -> SourceError {
    SourceError::ColumnType {
        query: at.query,
        column: at.number,
        role: at.role,
        expected,
        found: found.name.clone(),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use super::*;

    fn column(oid: pg_sys::Oid, name: &str) -> ColumnType {
        ColumnType { oid, name: name.to_owned(), nondeterministic: None }
    }

    fn text() -> ColumnType {
        column(pg_sys::TEXTOID, "text")
    }

    fn varchar() -> ColumnType {
        column(pg_sys::VARCHAROID, "character varying")
    }

    fn case_insensitive(column: ColumnType) -> ColumnType {
        ColumnType { nondeterministic: Some("und-ci".to_owned()), ..column }
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
        assert_eq!(check_node_columns(&[text()]), Ok(IdType::Text));
        assert_eq!(check_node_columns(&[varchar()]), Ok(IdType::Text));
        assert_eq!(
            check_node_columns(&[column(pg_sys::FLOAT8OID, "double precision")]),
            Err(SourceError::ColumnType {
                query: "node_query",
                column: 1,
                role: "node id",
                expected: "integer, bigint, uuid or text",
                found: "double precision".to_owned(),
            })
        );
    }

    #[test]
    fn text_like_types_whose_equality_is_not_bytewise_are_refused_with_their_cast() {
        let bpchar = column(pg_sys::BPCHAROID, "character(8)");
        let name = column(pg_sys::NAMEOID, "name");
        let citext = column(pg_sys::Oid::from(90_000), "public.citext");
        for (refused, cast) in [(&bpchar, "id::text"), (&name, "id::text")] {
            let error = check_node_columns(std::slice::from_ref(refused)).unwrap_err();
            assert!(matches!(error, SourceError::IdTypeRefused { cast: c, .. } if c == cast));
        }
        let error = check_node_columns(std::slice::from_ref(&citext)).unwrap_err();
        assert!(error.to_string().contains("ignores case"), "{error}");
        assert!(error.to_string().contains("lower(id)::text"), "{error}");
        let error = check_edge_columns(&[text(), bpchar], IdType::Text, false, false).unwrap_err();
        assert!(matches!(error, SourceError::IdTypeRefused { column: 2, .. }), "{error:?}");
        assert_eq!(error.sqlstate(), PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH);
    }

    #[test]
    fn a_text_id_column_with_a_nondeterministic_collation_is_refused() {
        let error = check_node_columns(&[case_insensitive(text())]).unwrap_err();
        assert_eq!(
            error,
            SourceError::NondeterministicCollation {
                query: "node_query",
                column: 1,
                role: "node id",
                collation: "und-ci".to_owned(),
            }
        );
        assert!(error.to_string().contains("COLLATE \"C\""), "{error}");
        assert_eq!(error.sqlstate(), PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH);
        let edges = [text(), case_insensitive(varchar())];
        assert!(matches!(
            check_edge_columns(&edges, IdType::Text, false, false),
            Err(SourceError::NondeterministicCollation { query: "edge_query", column: 2, .. })
        ));
        // A relation's collation is not an id's.
        let relation = case_insensitive(text());
        assert_eq!(
            check_edge_columns(&[text(), text(), relation], IdType::Text, true, false),
            Ok(())
        );
    }

    #[test]
    fn text_and_varchar_endpoints_mix_on_a_text_graph() {
        assert_eq!(check_edge_columns(&[text(), varchar()], IdType::Text, false, false), Ok(()));
        assert_eq!(require_node_id_type(&[varchar()], IdType::Text), Ok(()));
        assert_eq!(
            check_edge_columns(&[text(), int8()], IdType::Text, false, false),
            Err(SourceError::ColumnType {
                query: "edge_query",
                column: 2,
                role: "target id",
                expected: "text",
                found: "bigint".to_owned(),
            })
        );
        assert!(check_edge_columns(&[varchar(), varchar()], IdType::Int8, false, false).is_err());
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
