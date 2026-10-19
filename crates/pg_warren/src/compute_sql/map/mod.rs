//! `warren.vector_map` and `warren.explain_map` (DESIGN.md §6.3): an
//! elementwise SQL expression over the version's vectors, parsed by
//! Postgres against a synthetic row of them; compiled to `warren-core`'s
//! IR and run across the compute threads when every operation in it is in
//! the compiled subset, otherwise evaluated by the executor, element by
//! element. Both paths give the same values, bit for bit, and the same
//! errors.

mod executor;
mod parse;
#[cfg(any(test, feature = "pg_test"))]
mod tests;
mod translate;

use pgrx::prelude::*;
use pgrx::JsonB;
use serde_json::json;
use warren_core::compute::{map, AnyVector};

use super::settings::workers;
use super::vectors::{bound_names, BoundName, Vectors};
use super::{entry, ComputeSqlError};
use crate::access::{with_search_path, Caller};
use crate::cache::resolve;
use crate::interrupts::PostgresInterrupts;
use crate::source::{with_id_type, IdDatum};
use parse::{analyse, Analysed, Column};
use translate::{translate, Compiled};

#[cfg(any(test, feature = "pg_test"))]
thread_local! {
    /// Test hook: evaluate every expression with the executor.
    static FORCE_EXECUTOR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test hook: makes `vector_map` use the executor for every expression.
#[cfg(any(test, feature = "pg_test"))]
pub(crate) fn force_executor(forced: bool) {
    FORCE_EXECUTOR.set(forced);
}

#[cfg(any(test, feature = "pg_test"))]
use executor::last_was_jit_compiled;

/// Whether the test hook forces the executor.
fn executor_forced() -> bool {
    #[cfg(any(test, feature = "pg_test"))]
    return FORCE_EXECUTOR.get();
    #[cfg(not(any(test, feature = "pg_test")))]
    false
}

/// The synthetic row's columns: the vectors bound to the version.
fn columns(names: &[BoundName]) -> Vec<Column> {
    names.iter().map(|bound| Column { name: bound.name.clone(), dtype: bound.dtype }).collect()
}

/// `expr` parsed against `columns`, and how it is evaluated.
fn plan(
    caller: &Caller,
    expr: &str,
    columns: Vec<Column>,
) -> Result<(Analysed, Compiled), ComputeSqlError> {
    let analysed = analyse(expr, columns, caller.search_path())?;
    let compiled = translate(&analysed);
    Ok((analysed, compiled))
}

/// `warren.vector_map`: vector `y` (replacing any of that name; transient
/// if not `persist`) holds
/// `expr` evaluated at every element, over the vectors bound to `version`
/// (the current version if NULL) by name. Its dtype is the expression's
/// type (`integer` and `smallint` as `i64`, `numeric` as `f64`), and its
/// width the widest operand's.
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn vector_map(
    graph: Option<&str>,
    y: Option<&str>,
    expr: Option<&str>,
    persist: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
) {
    entry(|caller| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let y = y.ok_or(ComputeSqlError::NullArgument("y"))?;
        let expr = expr.ok_or(ComputeSqlError::NullArgument("expr"))?;
        let persist = persist.ok_or(ComputeSqlError::NullArgument("persist"))?;
        let resolved = resolve(graph, version, "write vectors of")?;
        with_id_type!(resolved.graph.id_type, E => {
            mapped::<E>(caller, Vectors::<E>::open(resolved)?, y, expr, persist)
        })
    })
}

fn mapped<E: IdDatum>(
    caller: &Caller,
    mut vectors: Vectors<E>,
    y: &str,
    expr: &str,
    persist: bool,
) -> Result<(), ComputeSqlError> {
    let (analysed, compiled) = plan(caller, expr, columns(&vectors.names()?))?;
    let id_space = vectors.id_space();
    let operands = |vectors: &mut Vectors<E>, columns: &[usize]| {
        columns
            .iter()
            .map(|&column| vectors.get(&analysed.columns[column].name))
            .collect::<Result<Vec<_>, _>>()
    };
    let result: AnyVector = match compiled {
        Ok((ir, columns)) if !executor_forced() => {
            let loaded = operands(&mut vectors, &columns)?;
            let refs: Vec<&AnyVector> = loaded.iter().map(|operand| &**operand).collect();
            map(&*workers()?, &vectors.memory, &mut PostgresInterrupts, id_space, &ir, &refs)?
        }
        _ => {
            let columns = executor::referenced(&analysed);
            let loaded = operands(&mut vectors, &columns)?;
            let refs: Vec<&AnyVector> = loaded.iter().map(|operand| &**operand).collect();
            // Function bodies, PL/pgSQL statements and regclass casts the
            // expression calls resolve names when they run, so they run
            // under the caller's search_path, as the caller's own SQL would.
            with_search_path(caller.search_path(), || {
                executor::evaluate(&analysed, &columns, &refs, id_space, &vectors.memory)
            })?
        }
    };
    vectors.save(y, result, persist)?;
    Ok(())
}

/// `warren.explain_map`: `{"path": "compiled" | "executor", "reason":
/// …, "dtype": …}`: how `vector_map` would evaluate `expr` over the
/// vectors bound to `version`, and why.
#[pg_extern(schema = "warren", stable, parallel_safe)]
fn explain_map(
    graph: Option<&str>,
    expr: Option<&str>,
    version: default!(Option<i64>, "NULL"),
) -> JsonB {
    entry(|caller| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let expr = expr.ok_or(ComputeSqlError::NullArgument("expr"))?;
        let resolved = resolve(graph, version, "read")?;
        let names = bound_names(resolved.graph.graph_id, resolved.version)?;
        let (analysed, compiled) = plan(caller, expr, columns(&names))?;
        let dtype = analysed.dtype.name();
        Ok(JsonB(match compiled {
            Ok(_) => json!({
                "path": "compiled",
                "reason": "every operation is in the compiled subset",
                "dtype": dtype,
            }),
            Err(reason) => json!({ "path": "executor", "reason": reason, "dtype": dtype }),
        }))
    })
}
