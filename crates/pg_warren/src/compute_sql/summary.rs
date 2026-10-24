//! Summaries of a vector's live nodes (DESIGN.md §6.2): `vector_topk` and
//! `vector_stats`.

use std::sync::Arc;

use pgrx::prelude::*;
use pgrx::{AnyElement, JsonB};
use serde_json::{json, Value};
use warren_core::compute::{summarise, top_k};

use super::vectors::Vectors;
use super::{entry, ComputeSqlError};
use crate::cache::resolve;
use crate::external_ids::{check_type, external};
use crate::interrupts::workers;
use crate::interrupts::PostgresInterrupts;
use crate::source::with_id_type;

/// `warren.vector_topk`: the `k` live nodes with the largest values of the
/// width-1 vector `name` (the smallest unless `descending`), best first,
/// ties by dense id. `NaN` ranks above every number.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.vector_topk(graph text, name text, k int, id_type anyelement,
                                   descending bool DEFAULT true, version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, value float8)
STABLE PARALLEL RESTRICTED LANGUAGE c AS 'MODULE_PATHNAME', 'vector_topk_wrapper';
"#
)]
fn vector_topk(
    graph: Option<&str>,
    name: Option<&str>,
    k: Option<i32>,
    _id_type: Option<AnyElement>,
    descending: Option<bool>,
    version: Option<i64>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> TableIterator<'static, (name!(node_id, AnyElement), name!(value, f64))> {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let name = name.ok_or(ComputeSqlError::NullArgument("name"))?;
        let k = k.ok_or(ComputeSqlError::NullArgument("k"))?;
        let k = u64::try_from(k)
            .map_err(|_| ComputeSqlError::InvalidArgument("k must not be negative".into()))?;
        let descending = descending.ok_or(ComputeSqlError::NullArgument("descending"))?;
        let resolved = resolve(graph, version, "read")?;
        let rows: Box<dyn Iterator<Item = (AnyElement, f64)>> = with_id_type!(resolved.graph.id_type, E => {
            // SAFETY: fcinfo is this call's, and argument 3 exists.
            let oid = unsafe { pgrx::fcinfo::pg_getarg_type(fcinfo, 3) };
            check_type::<E>(graph, "id_type", oid, false)?;
            let mut vectors = Vectors::<E>::open(resolved)?;
            let vector = vectors.get(name)?;
            let ranked = top_k(
                &vectors.memory,
                &mut PostgresInterrupts,
                &vector,
                vectors.tombstones(),
                k,
                descending,
            )?;
            // Each row's external id is made when it is returned (DESIGN.md
            // §12), the ranking staying in its buffer.
            let loaded = Arc::clone(&vectors.loaded);
            Box::new(ranked.into_iter().filter_map(move |(node, value)| {
                external(&loaded.id_map, node).map(|id| (id, value))
            }))
        });
        Ok(TableIterator::new(rows))
    })
}

/// A statistic as JSON: a number, or for a non-finite value the text
/// `double precision` reads it from (`NaN`, `Infinity`, `-Infinity`), since
/// JSON has no such numbers.
fn number(value: Option<f64>) -> Value {
    match value {
        None => Value::Null,
        Some(value) if value.is_finite() => json!(value),
        Some(value) if value.is_nan() => json!("NaN"),
        Some(value) if value > 0.0 => json!("Infinity"),
        Some(_) => json!("-Infinity"),
    }
}

/// `warren.vector_stats`: `{count, min, max, mean, sd, l2, nnz}` over every
/// element of the live rows of vector `name`. `sd` is the sample standard
/// deviation (SQL `stddev`); undefined statistics are null.
#[pg_extern(schema = "warren", stable, parallel_restricted)]
fn vector_stats(
    graph: Option<&str>,
    name: Option<&str>,
    version: default!(Option<i64>, "NULL"),
) -> JsonB {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let name = name.ok_or(ComputeSqlError::NullArgument("name"))?;
        let resolved = resolve(graph, version, "read")?;
        let summary = with_id_type!(resolved.graph.id_type, E => {
            let mut vectors = Vectors::<E>::open(resolved)?;
            let vector = vectors.get(name)?;
            summarise(&*workers()?, &mut PostgresInterrupts, &vector, vectors.tombstones())?
        });
        Ok(JsonB(json!({
            "count": summary.count,
            "min": number(summary.min),
            "max": number(summary.max),
            "mean": number(summary.mean),
            "sd": number(summary.sd),
            "l2": number(Some(summary.l2)),
            "nnz": summary.nnz,
        })))
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{held_after_first_row, iri_path, refresh, rows, small};

    /// Each ranked node's text id is made when its row is returned, so a
    /// call holds no more Postgres memory for 50,000 rows to come than for
    /// 1,000.
    #[pg_test]
    fn the_top_k_makes_each_rows_id_as_it_is_returned() -> Result<(), Error> {
        for (graph, nodes) in [("few", 1_000), ("many", 50_000)] {
            iri_path(graph, nodes)?;
            Spi::run(&format!(
                "SELECT warren.vector_from_query('{graph}', 'v', 'SELECT id, n::float8 FROM {graph}_nodes')"
            ))?;
        }
        let few = held_after_first_row("warren.vector_topk('few', 'v', 50000, NULL::text)")?;
        let many = held_after_first_row("warren.vector_topk('many', 'v', 50000, NULL::text)")?;
        assert!(many < few + (256 << 10), "{many} bytes held for 50,000 rows, {few} for 1,000");
        Ok(())
    }

    #[pg_test]
    fn top_k_ranks_live_nodes_with_ties_by_dense_id() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT warren.vector_degree('t', 'in', 'in')")?;
        let top = rows(
            "SELECT node_id || ':' || value FROM warren.vector_topk('t', 'in', 3, NULL::bigint)",
        )?;
        assert_eq!(top.as_deref(), Some("4:2 1:1 2:1"));
        let bottom = rows(
            "SELECT node_id || ':' || value
             FROM warren.vector_topk('t', 'in', 2, NULL::bigint, descending := false)",
        )?;
        assert_eq!(bottom.as_deref(), Some("6:0 1:1"));
        Ok(())
    }

    #[pg_test]
    fn stats_summarise_live_nodes_and_match_sql_aggregates() -> Result<(), Error> {
        small("t")?;
        Spi::run("DELETE FROM t_nodes WHERE id = 4")?;
        refresh("t", true)?;
        Spi::run("SELECT warren.vector_dense_id('t', 'id', 'f64')")?;
        // sd is combined pairwise (Chan et al.), so it may differ from
        // stddev's running sums in the last bit.
        let same = Spi::get_one::<bool>(
            "SELECT s - 'sd' = jsonb_build_object('count', count(*), 'min', min(value),
                        'max', max(value), 'mean', avg(value), 'l2', sqrt(sum(value * value)),
                        'nnz', count(*) FILTER (WHERE value <> 0))
                    AND abs((s ->> 'sd')::float8 - stddev(value)) < 1e-12
             FROM warren.vector_values('t', 'id', NULL::bigint),
                  warren.vector_stats('t', 'id') s
             GROUP BY s",
        )?;
        assert_eq!(same, Some(true));
        Ok(())
    }

    #[pg_test]
    fn stats_of_non_finite_values_are_text() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT warren.vector_const('t', 'nan', 'NaN'::float8)")?;
        let stats = Spi::get_one::<String>(
            "SELECT (warren.vector_stats('t', 'nan') ->> 'mean')
                    || ' ' || (warren.vector_stats('t', 'nan') ->> 'count')",
        )?;
        assert_eq!(stats.as_deref(), Some("NaN 6"));
        Ok(())
    }
}
