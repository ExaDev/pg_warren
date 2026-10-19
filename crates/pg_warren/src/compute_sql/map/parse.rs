//! Parsing a `vector_map` expression with Postgres's own parser, against a
//! synthetic row whose columns are the version's vectors (DESIGN.md §6.3):
//!
//! ```sql
//! SELECT (<expr>)[::<result type>] FROM (VALUES (NULL::float8, ...)) AS v("pr", "deg", ...)
//! ```
//!
//! The expression is first parsed alone, as `SELECT <expr>`, and must be a
//! single target with no other clause, so it cannot reach outside the
//! synthetic row. Parse analysis runs under the caller's `search_path`, as
//! any of the caller's own SQL does. A result of type `integer` or
//! `smallint` is cast to `bigint`, and a `numeric` one to `double
//! precision`, as an assignment to a vector of that dtype would; the cast
//! is part of the analysed tree both evaluation paths run.

use std::ffi::{CStr, CString};

use pgrx::{pg_sys, PgList};
use warren_core::compute::Dtype;

use super::super::ComputeSqlError;
use crate::access::with_search_path;

/// A column of the synthetic row: a vector bound to the version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Column {
    /// The vector's name, the column's name.
    pub(crate) name: String,
    /// The vector's dtype, whose SQL type is the column's type.
    pub(crate) dtype: Dtype,
}

/// An analysed expression: Postgres's tree for it (in the call's memory
/// context), its result dtype, and the columns its `Var`s number from 1.
pub(crate) struct Analysed {
    /// The expression tree.
    pub(crate) expr: *mut pg_sys::Node,
    /// The result's dtype.
    pub(crate) dtype: Dtype,
    /// The synthetic row's columns.
    pub(crate) columns: Vec<Column>,
}

/// The SQL type of a dtype, schema-qualified.
pub(crate) fn sql_type(dtype: Dtype) -> &'static str {
    match dtype {
        Dtype::F64 => "pg_catalog.float8",
        Dtype::F32 => "pg_catalog.float4",
        Dtype::I64 => "pg_catalog.int8",
        Dtype::Bool => "pg_catalog.bool",
    }
}

/// The dtype of a SQL type a vector can hold.
pub(crate) fn dtype_of(oid: pg_sys::Oid) -> Option<Dtype> {
    match oid {
        pg_sys::FLOAT8OID => Some(Dtype::F64),
        pg_sys::FLOAT4OID => Some(Dtype::F32),
        pg_sys::INT8OID => Some(Dtype::I64),
        pg_sys::BOOLOID => Some(Dtype::Bool),
        _ => None,
    }
}

/// `text` as a C string.
fn c_string(text: &str) -> Result<CString, ComputeSqlError> {
    CString::new(text)
        .map_err(|_| ComputeSqlError::MapSyntax("the expression contains a NUL byte".into()))
}

/// `name` quoted as an identifier where it needs to be.
fn quoted(name: &str) -> Result<String, ComputeSqlError> {
    let name = c_string(name)?;
    // SAFETY: quote_identifier reads a NUL-terminated string and returns it
    // or a palloc'd quoted copy, NUL-terminated.
    let quoted = unsafe { CStr::from_ptr(pg_sys::quote_identifier(name.as_ptr())) };
    Ok(quoted.to_string_lossy().into_owned())
}

/// The statements `sql` parses to.
fn raw_parse(sql: &CString) -> PgList<pg_sys::RawStmt> {
    // SAFETY: `sql` is NUL-terminated and outlives the call; a syntax
    // error raises an ERROR rather than return.
    let list = unsafe { pg_sys::raw_parser(sql.as_ptr(), pg_sys::RawParseMode::RAW_PARSE_DEFAULT) };
    // SAFETY: raw_parser returns a List of RawStmt (or NIL).
    unsafe { PgList::from_pg(list) }
}

/// Checks that `expr` is one expression: `SELECT <expr>` parses to one
/// SELECT of one target and no other clause.
fn check_lone_expression(expr: &str) -> Result<(), ComputeSqlError> {
    let sql = c_string(&format!("SELECT\n{expr}\n"))?;
    let statements = raw_parse(&sql);
    let not_one = || {
        ComputeSqlError::MapSyntax(
            "a vector_map expression must be one SQL expression, with no FROM, WHERE or other \
             clause"
                .into(),
        )
    };
    let statement = match (statements.len(), statements.head()) {
        (1, Some(statement)) => statement,
        _ => return Err(not_one()),
    };
    // SAFETY: a RawStmt from raw_parser; its stmt is a parse node.
    let node = unsafe { (*statement).stmt };
    // SAFETY: `node` is a valid parse node.
    if !unsafe { pgrx::is_a(node, pg_sys::NodeTag::T_SelectStmt) } {
        return Err(not_one());
    }
    let select = node.cast::<pg_sys::SelectStmt>();
    // SAFETY: checked to be a SelectStmt above.
    let s = unsafe { &*select };
    // SAFETY: targetList is a List (or NIL).
    let targets = unsafe { PgList::<pg_sys::Node>::from_pg(s.targetList) }.len();
    let clauses = [
        s.distinctClause,
        s.fromClause,
        s.groupClause,
        s.windowClause,
        s.valuesLists,
        s.sortClause,
        s.lockingClause,
    ];
    let lone = targets == 1
        && clauses.iter().all(|list| list.is_null())
        && s.intoClause.is_null()
        && s.whereClause.is_null()
        && s.havingClause.is_null()
        && s.limitOffset.is_null()
        && s.limitCount.is_null()
        && s.withClause.is_null()
        && s.op == pg_sys::SetOperation::SETOP_NONE;
    match lone {
        true => Ok(()),
        false => Err(not_one()),
    }
}

/// The synthetic query over `columns`, with the result cast to `cast`.
fn synthetic(
    expr: &str,
    columns: &[Column],
    cast: Option<&str>,
) -> Result<String, ComputeSqlError> {
    let cast = cast.map(|cast| format!("::{cast}")).unwrap_or_default();
    if columns.is_empty() {
        return Ok(format!("SELECT (\n{expr}\n){cast}"));
    }
    let nulls: Vec<String> =
        columns.iter().map(|column| format!("NULL::{}", sql_type(column.dtype))).collect();
    let names = columns.iter().map(|column| quoted(&column.name)).collect::<Result<Vec<_>, _>>()?;
    Ok(format!(
        "SELECT (\n{expr}\n){cast} FROM (VALUES ({})) AS v({})",
        nulls.join(", "),
        names.join(", ")
    ))
}

/// The analysed target expression of `sql`, and its type.
fn analyse_query(sql: &str) -> Result<(*mut pg_sys::Node, pg_sys::Oid), ComputeSqlError> {
    let text = c_string(sql)?;
    let statements = raw_parse(&text);
    let statement = statements.head().ok_or_else(|| ComputeSqlError::MapSyntax(sql.into()))?;
    // SAFETY: a RawStmt from raw_parser over `text`, which outlives the
    // call; no parameters. Analysis errors are raised as ERRORs.
    let query = unsafe {
        pg_sys::parse_analyze_fixedparams(
            statement,
            text.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: parse analysis returns a valid Query.
    let q = unsafe { &*query };
    if q.hasAggs || q.hasWindowFuncs || q.hasTargetSRFs || q.hasSubLinks {
        return Err(ComputeSqlError::MapUnsupported(
            "a vector_map expression cannot contain aggregates, window functions, set-returning \
             functions or subqueries",
        ));
    }
    // SAFETY: a SELECT's targetList is a List of TargetEntry.
    let targets = unsafe { PgList::<pg_sys::TargetEntry>::from_pg(q.targetList) };
    let target = targets.head().ok_or_else(|| ComputeSqlError::MapSyntax(sql.into()))?;
    // SAFETY: a TargetEntry's expr is an expression node.
    let expr = unsafe { (*target).expr }.cast::<pg_sys::Node>();
    // SAFETY: `expr` is a valid expression node.
    Ok((expr, unsafe { pg_sys::exprType(expr) }))
}

/// Parses and analyses `expr` against `columns` under `search_path`.
pub(crate) fn analyse(
    expr: &str,
    columns: Vec<Column>,
    search_path: &str,
) -> Result<Analysed, ComputeSqlError> {
    with_search_path(search_path, || {
        check_lone_expression(expr)?;
        let (mut tree, mut oid) = analyse_query(&synthetic(expr, &columns, None)?)?;
        let cast = match oid {
            pg_sys::INT2OID | pg_sys::INT4OID => Some("pg_catalog.int8"),
            pg_sys::NUMERICOID => Some("pg_catalog.float8"),
            _ => None,
        };
        if let Some(cast) = cast {
            (tree, oid) = analyse_query(&synthetic(expr, &columns, Some(cast))?)?;
        }
        let dtype =
            dtype_of(oid).ok_or_else(|| ComputeSqlError::MapType(crate::source::type_name(oid)))?;
        Ok(Analysed { expr: tree, dtype, columns })
    })
}
