//! Reading vectors back as rows (DESIGN.md §6.2): `vector_values`,
//! `matrix_rows`, `vector_topk`, `vector_stats` and `id_map`. Rows are the
//! live nodes, in dense id order, by external id.
//!
//! These functions have no id argument to take the result's id type from,
//! so, like `warren.components`, they take a type witness: pass
//! `NULL::<the graph's id type>` as `id_type`.

use std::sync::Arc;

use pgrx::prelude::*;
use pgrx::AnyElement;
use warren_core::compute::AnyVector;
use warren_core::graph::DenseId;

use super::vectors::Vectors;
use super::{entry, ComputeSqlError};
use crate::cache::{resolve, LoadedVersion};
use crate::source::{with_id_type, IdDatum};
use crate::traversal_sql::{check_type, external};

/// Element `index` of `vector` as a `double precision`.
pub(super) fn element(vector: &AnyVector, index: usize) -> f64 {
    use warren_core::compute::Element;
    match vector {
        AnyVector::F32(vector) => vector.values()[index].to_f64(),
        AnyVector::F64(vector) => vector.values()[index],
        AnyVector::I64(vector) => vector.values()[index].to_f64(),
        AnyVector::Bool(vector) => vector.values()[index].to_f64(),
    }
}

/// The live nodes of `loaded` with their external ids, in dense id order.
fn live<E: IdDatum>(loaded: Arc<LoadedVersion<E>>) -> impl Iterator<Item = (DenseId, AnyElement)> {
    (0..loaded.id_map.id_space()).filter_map(move |node| {
        match loaded.id_map.tombstones().contains(node) {
            true => None,
            false => external(&loaded.id_map, node).map(|id| (node, id)),
        }
    })
}

/// Opens vector `name` of `graph` at `version` for reading as rows,
/// checking the id type witness (argument `witness` of `fcinfo`).
fn open<E: IdDatum>(
    graph: &str,
    name: &str,
    version: Option<i64>,
    fcinfo: pg_sys::FunctionCallInfo,
    witness: usize,
) -> Result<(Arc<LoadedVersion<E>>, Arc<AnyVector>), ComputeSqlError> {
    // SAFETY: fcinfo is this call's, and argument `witness` exists.
    let oid = unsafe { pgrx::fcinfo::pg_getarg_type(fcinfo, witness) };
    check_type::<E>(graph, "id_type", oid, false)?;
    let mut vectors = Vectors::<E>::open(resolve(graph, version, "read")?)?;
    let vector = vectors.get(name)?;
    Ok((Arc::clone(&vectors.loaded), vector))
}

/// `warren.vector_values`: each live node with its value of the width-1
/// vector `name` as a `double precision`.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.vector_values(graph text, name text, id_type anyelement,
                                     version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, value float8)
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'vector_values_wrapper';
"#
)]
fn vector_values(
    graph: Option<&str>,
    name: Option<&str>,
    _id_type: Option<AnyElement>,
    version: Option<i64>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> TableIterator<'static, (name!(node_id, AnyElement), name!(value, f64))> {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let name = name.ok_or(ComputeSqlError::NullArgument("name"))?;
        let resolved = resolve(graph, version, "read")?;
        let rows: Box<dyn Iterator<Item = (AnyElement, f64)>> = with_id_type!(resolved.graph.id_type, E => {
            let (loaded, vector) = open::<E>(graph, name, version, fcinfo, 2)?;
            if vector.width() != 1 {
                return Err(ComputeSqlError::InvalidArgument(format!(
                    "vector \"{name}\" has width {}; read it with warren.matrix_rows",
                    vector.width()
                )));
            }
            Box::new(live(loaded).map(move |(node, id)| (id, element(&vector, node as usize))))
        });
        Ok(TableIterator::new(rows))
    })
}

/// `warren.matrix_rows`: each live node with its row of vector `name` as
/// a `double precision[]` (cast it to `vector` for pgvector).
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.matrix_rows(graph text, name text, id_type anyelement,
                                   version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, "row" float8[])
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'matrix_rows_wrapper';
"#
)]
fn matrix_rows(
    graph: Option<&str>,
    name: Option<&str>,
    _id_type: Option<AnyElement>,
    version: Option<i64>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> TableIterator<'static, (name!(node_id, AnyElement), name!(row, Vec<f64>))> {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let name = name.ok_or(ComputeSqlError::NullArgument("name"))?;
        let resolved = resolve(graph, version, "read")?;
        let rows: Box<dyn Iterator<Item = (AnyElement, Vec<f64>)>> = with_id_type!(resolved.graph.id_type, E => {
            let (loaded, vector) = open::<E>(graph, name, version, fcinfo, 2)?;
            let width = vector.width();
            Box::new(live(loaded).map(move |(node, id)| {
                let start = node as usize * width;
                (id, (start..start + width).map(|i| element(&vector, i)).collect())
            }))
        });
        Ok(TableIterator::new(rows))
    })
}

/// `warren.id_map`: every node of the version, tombstoned ones included,
/// with its dense id and whether it is live.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.id_map(graph text, id_type anyelement, version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, dense_id bigint, live boolean)
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'id_map_wrapper';
"#
)]
fn id_map(
    graph: Option<&str>,
    _id_type: Option<AnyElement>,
    version: Option<i64>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> TableIterator<'static, (name!(node_id, AnyElement), name!(dense_id, i64), name!(live, bool))> {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let resolved = resolve(graph, version, "read")?;
        let rows: Box<dyn Iterator<Item = (AnyElement, i64, bool)>> = with_id_type!(resolved.graph.id_type, E => {
            // SAFETY: fcinfo is this call's, and argument 1 exists.
            let oid = unsafe { pgrx::fcinfo::pg_getarg_type(fcinfo, 1) };
            check_type::<E>(graph, "id_type", oid, false)?;
            let loaded = crate::cache::cached::<E>(resolved.graph.graph_id, resolved.version)?;
            Box::new((0..loaded.id_map.id_space()).filter_map(move |node| {
                let live = !loaded.id_map.tombstones().contains(node);
                let dense = i64::try_from(node).unwrap_or(i64::MAX);
                external(&loaded.id_map, node).map(|id| (id, dense, live))
            }))
        });
        Ok(TableIterator::new(rows))
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, refresh, rows, small};

    #[pg_test]
    fn a_constant_vector_reads_back_at_every_live_node() -> Result<(), Error> {
        small("t")?;
        let id = Spi::get_one::<i64>("SELECT warren.vector_const('t', 'c', 2.5)")?;
        assert!(id.is_some());
        let found = rows(
            "SELECT node_id || ':' || value FROM warren.vector_values('t', 'c', NULL::bigint)",
        )?;
        assert_eq!(found.as_deref(), Some("1:2.5 2:2.5 3:2.5 4:2.5 5:2.5 6:2.5"));
        Ok(())
    }

    #[pg_test]
    fn a_constant_is_cast_to_the_dtype_as_sql_casts_it() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "SELECT warren.vector_const('t', 'i', 2.5, 'i64');
             SELECT warren.vector_const('t', 'b', 0.0, 'bool');
             SELECT warren.vector_const('t', 'f', 0.1, 'f32')",
        )?;
        let found = rows(
            "SELECT i.value || ' ' || b.value || ' ' || f.value
             FROM warren.vector_values('t', 'i', NULL::bigint) i
             JOIN warren.vector_values('t', 'b', NULL::bigint) b USING (node_id)
             JOIN warren.vector_values('t', 'f', NULL::bigint) f USING (node_id)
             WHERE node_id = 1",
        )?;
        let f32_tenth = f64::from(0.1f32);
        assert_eq!(found, Some(format!("2 0 {f32_tenth}")));
        Ok(())
    }

    #[pg_test]
    fn tombstoned_nodes_are_not_read_back_and_the_id_map_marks_them() -> Result<(), Error> {
        small("t")?;
        Spi::run("DELETE FROM t_nodes WHERE id = 2")?;
        refresh("t", true)?;
        Spi::run("SELECT warren.vector_const('t', 'c', 1)")?;
        let found = rows("SELECT node_id::text FROM warren.vector_values('t', 'c', NULL::bigint)")?;
        assert_eq!(found.as_deref(), Some("1 3 4 5 6"));
        let map = rows(
            "SELECT node_id || ':' || dense_id || ':' || live FROM warren.id_map('t', NULL::bigint)",
        )?;
        assert_eq!(map.as_deref(), Some("1:0:true 2:1:false 3:2:true 4:3:true 5:4:true 6:5:true"));
        Ok(())
    }

    #[pg_test]
    fn matrix_rows_read_whole_rows() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT warren.vector_const('t', 'c', 3)")?;
        let found = rows(
            "SELECT node_id || ':' || row::text FROM warren.matrix_rows('t', 'c', NULL::bigint)
             WHERE node_id < 3",
        )?;
        assert_eq!(found.as_deref(), Some("1:{3} 2:{3}"));
        Ok(())
    }

    #[pg_test]
    fn reading_an_unknown_vector_fails() -> Result<(), Error> {
        let version = small("t")?;
        let found =
            failure("(SELECT count(*) FROM warren.vector_values('t', 'nope', NULL::bigint))")?;
        let expected = format!("42704: graph \"t\" has no vector \"nope\" at version {version}");
        assert_eq!(found, Some(expected));
        Ok(())
    }

    #[pg_test(
        error = "graph \"t\" has node ids of type bigint, but id_type is of type integer; cast it to bigint"
    )]
    fn vector_values_checks_the_id_type_witness() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT warren.vector_const('t', 'c', 1)")?;
        Spi::run("SELECT * FROM warren.vector_values('t', 'c', NULL::int)")
    }
}
