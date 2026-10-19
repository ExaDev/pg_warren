//! Vectors made from the graph alone (DESIGN.md §6.2): `vector_const`,
//! `vector_degree` and `vector_dense_id`.

use pgrx::prelude::*;
use warren_core::compute::{map, AnyExpr, Dtype, Expr};

use super::settings::workers;
use super::vectors::Vectors;
use super::{entry, ComputeSqlError};
use crate::cache::resolve;
use crate::interrupts::PostgresInterrupts;
use crate::source::{with_id_type, IdDatum};

/// `value` as an expression of `dtype`: a `double precision` literal, cast
/// as SQL casts it.
pub(super) fn literal(value: f64, dtype: Dtype) -> AnyExpr {
    let literal = AnyExpr::F64(Expr::Literal(Some(value)));
    let cast = Box::new(literal.clone());
    match dtype {
        Dtype::F64 => literal,
        Dtype::F32 => AnyExpr::F32(Expr::Cast(cast)),
        Dtype::I64 => AnyExpr::I64(Expr::Cast(cast)),
        Dtype::Bool => AnyExpr::Bool(Expr::Cast(cast)),
    }
}

/// `dtype`, parsed.
pub(super) fn dtype(text: Option<&str>) -> Result<Dtype, ComputeSqlError> {
    Ok(text.ok_or(ComputeSqlError::NullArgument("dtype"))?.parse()?)
}

/// `warren.vector_const`: vector `name` of `dtype`, `value` (cast as SQL
/// casts `double precision`) at every live node, bound to `version` (the
/// current version if NULL). Returns its vector id.
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn vector_const(
    graph: Option<&str>,
    name: Option<&str>,
    value: Option<f64>,
    dtype: default!(Option<&str>, "'f64'"),
    version: default!(Option<i64>, "NULL"),
) -> i64 {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let name = name.ok_or(ComputeSqlError::NullArgument("name"))?;
        let value = value.ok_or(ComputeSqlError::NullArgument("value"))?;
        let expr = literal(value, self::dtype(dtype)?);
        let resolved = resolve(graph, version, "write vectors of")?;
        with_id_type!(resolved.graph.id_type, E => constant::<E>(Vectors::open(resolved)?, name, &expr))
    })
}

fn constant<E: IdDatum>(
    mut vectors: Vectors<E>,
    name: &str,
    expr: &AnyExpr,
) -> Result<i64, ComputeSqlError> {
    let filled =
        map(&*workers()?, &vectors.memory, &mut PostgresInterrupts, vectors.id_space(), expr, &[])?;
    vectors.save(name, filled)
}
