//! Vectors made from the graph alone (DESIGN.md §6.2, §6.7):
//! `vector_const`, `vector_degree`, `vector_dense_id` and `triangles`.

use pgrx::prelude::*;
use warren_core::compute::{self, map, AnyExpr, AnyVector, Dtype, Expr, Vector};
use warren_core::graph::RelationSelection;

use super::settings::workers;
use super::vectors::Vectors;
use super::{entry, ComputeSqlError};
use crate::cache::resolve;
use crate::interrupts::PostgresInterrupts;
use crate::selection::{adjacency, direction, relations};
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
/// current version if NULL). Returns its vector id (NULL if not
/// `persist`: a transient vector, §6.2).
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn vector_const(
    graph: Option<&str>,
    name: Option<&str>,
    value: Option<f64>,
    dtype: default!(Option<&str>, "'f64'"),
    persist: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
) -> Option<i64> {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let name = name.ok_or(ComputeSqlError::NullArgument("name"))?;
        let persist = persist.ok_or(ComputeSqlError::NullArgument("persist"))?;
        let value = value.ok_or(ComputeSqlError::NullArgument("value"))?;
        let expr = literal(value, self::dtype(dtype)?);
        let resolved = resolve(graph, version, "write vectors of")?;
        with_id_type!(resolved.graph.id_type, E => constant::<E>(Vectors::open(resolved)?, name, &expr, persist))
    })
}

fn constant<E: IdDatum>(
    mut vectors: Vectors<E>,
    name: &str,
    expr: &AnyExpr,
    persist: bool,
) -> Result<Option<i64>, ComputeSqlError> {
    let filled =
        map(&*workers()?, &vectors.memory, &mut PostgresInterrupts, vectors.id_space(), expr, &[])?;
    vectors.save(name, filled, persist)
}

/// Nodes between interrupt checks while counting degrees.
const POLL_NODES: u64 = 1 << 16;

/// `vector`, an `i64` vector, cast to `dtype` as SQL casts `bigint`.
fn cast<E: IdDatum>(
    vectors: &Vectors<E>,
    vector: AnyVector,
    dtype: Dtype,
) -> Result<AnyVector, ComputeSqlError> {
    let operand = Box::new(AnyExpr::I64(Expr::Vector(0)));
    let expr = match dtype {
        Dtype::I64 => return Ok(vector),
        Dtype::F64 => AnyExpr::F64(Expr::Cast(operand)),
        Dtype::F32 => AnyExpr::F32(Expr::Cast(operand)),
        Dtype::Bool => AnyExpr::Bool(Expr::Cast(operand)),
    };
    let id_space = vectors.id_space();
    Ok(map(&*workers()?, &vectors.memory, &mut PostgresInterrupts, id_space, &expr, &[&vector])?)
}

/// `warren.vector_degree`: vector `name` of `dtype` holding each node's
/// degree in `direction` (`out`, `in` or `both`) over the relations `rels`
/// (all if NULL): its distinct neighbours, as `warren.degree` counts them.
/// Returns its vector id (NULL if not `persist`: a transient vector).
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn vector_degree(
    graph: Option<&str>,
    name: Option<&str>,
    direction: default!(Option<&str>, "'out'"),
    rels: default!(Option<Vec<Option<String>>>, "NULL"),
    dtype: default!(Option<&str>, "'f64'"),
    persist: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
) -> Option<i64> {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let name = name.ok_or(ComputeSqlError::NullArgument("name"))?;
        let persist = persist.ok_or(ComputeSqlError::NullArgument("persist"))?;
        let direction = self::direction(direction)?;
        let dtype = self::dtype(dtype)?;
        let resolved = resolve(graph, version, "write vectors of")?;
        with_id_type!(resolved.graph.id_type, E => {
            let mut vectors = Vectors::<E>::open(resolved)?;
            let loaded = &vectors.loaded;
            let adjacency =
                adjacency(&loaded.graph, &loaded.relations, direction, rels.as_deref())?;
            let id_space = vectors.id_space();
            let mut degrees = Vector::filled(&vectors.memory, id_space, 1, 0i64)?;
            for (node, degree) in (0..).zip(degrees.values_mut()) {
                if node % POLL_NODES == 0 {
                    check_for_interrupts!();
                }
                *degree = i64::try_from(adjacency.degree(node)).unwrap_or(i64::MAX);
            }
            let degrees = cast(&vectors, degrees.into(), dtype)?;
            vectors.save(name, degrees, persist)
        })
    })
}

/// `warren.vector_dense_id`: vector `name` of `dtype` holding each node's
/// own dense id, cast as SQL casts `bigint` (seeds label propagation).
/// Returns its vector id (NULL if not `persist`: a transient vector).
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn vector_dense_id(
    graph: Option<&str>,
    name: Option<&str>,
    dtype: default!(Option<&str>, "'i64'"),
    persist: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
) -> Option<i64> {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let name = name.ok_or(ComputeSqlError::NullArgument("name"))?;
        let persist = persist.ok_or(ComputeSqlError::NullArgument("persist"))?;
        let dtype = self::dtype(dtype)?;
        let resolved = resolve(graph, version, "write vectors of")?;
        with_id_type!(resolved.graph.id_type, E => {
            let mut vectors = Vectors::<E>::open(resolved)?;
            let mut ids = Vector::filled(&vectors.memory, vectors.id_space(), 1, 0i64)?;
            for (node, id) in (0..).zip(ids.values_mut()) {
                *id = node;
            }
            let ids = cast(&vectors, ids.into(), dtype)?;
            vectors.save(name, ids, persist)
        })
    })
}

/// `warren.triangles` (§6.7): vector `out` (`i64`) holding each node's
/// number of triangles in the undirected simple graph under the relations
/// `rels` (all if NULL): edge directions and self-loops are ignored, as
/// NetworkX's `triangles` does. Returns its vector id (NULL if not
/// `persist`).
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn triangles(
    graph: Option<&str>,
    out: Option<&str>,
    rels: default!(Option<Vec<Option<String>>>, "NULL"),
    persist: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
) -> Option<i64> {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let out = out.ok_or(ComputeSqlError::NullArgument("out"))?;
        let persist = persist.ok_or(ComputeSqlError::NullArgument("persist"))?;
        let resolved = resolve(graph, version, "write vectors of")?;
        with_id_type!(resolved.graph.id_type, E => {
            let mut vectors = Vectors::<E>::open(resolved)?;
            let loaded = &vectors.loaded;
            let selected = relations(&loaded.relations, rels.as_deref());
            let selection = selected.as_deref().map_or(RelationSelection::All, RelationSelection::Only);
            let counts = compute::triangles(
                &*workers()?,
                &vectors.memory,
                &mut PostgresInterrupts,
                &loaded.graph,
                selection,
            )?;
            vectors.save(out, counts.into(), persist)
        })
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, refresh, rows, small};

    fn values(name: &str) -> Result<Option<String>, Error> {
        rows(&format!(
            "SELECT node_id || ':' || value FROM warren.vector_values('t', '{name}', NULL::bigint)"
        ))
    }

    #[pg_test]
    fn reserved_names_are_only_transient() -> Result<(), Error> {
        small("t")?;
        let refused = failure("warren.vector_const('t', '_warren_x', 1)")?.unwrap_or_default();
        assert!(
            refused.starts_with("42939: vector names beginning with _warren_ are reserved"),
            "{refused}"
        );
        Spi::run("SELECT warren.vector_const('t', '_warren_x', 1, persist := false)")?;
        Ok(())
    }

    #[pg_test]
    fn triangles_counts_each_nodes_triangles_ignoring_direction() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE tri_nodes (id bigint);
             CREATE TABLE tri_edges (src bigint, dst bigint, rel text);
             INSERT INTO tri_nodes SELECT generate_series(1, 5);
             INSERT INTO tri_edges VALUES (1, 2, 'a'), (2, 3, 'a'), (3, 1, 'b'), (1, 3, 'a'),
                                          (3, 4, 'a'), (4, 4, 'a');
             SELECT warren.create_graph('tri', 'SELECT id FROM tri_nodes',
                                        'SELECT src, dst, rel FROM tri_edges', has_relations := true,
                                        build_reverse := false);
             SELECT warren.refresh('tri');
             SELECT warren.triangles('tri', 'all');
             SELECT warren.triangles('tri', 'a', ARRAY['a'])",
        )?;
        let counts = |name: &str| {
            rows(&format!(
                "SELECT node_id || ':' || value FROM warren.vector_values('tri', '{name}', NULL::bigint)"
            ))
        };
        assert_eq!(counts("all")?.as_deref(), Some("1:1 2:1 3:1 4:0 5:0"));
        assert_eq!(counts("a")?.as_deref(), Some("1:1 2:1 3:1 4:0 5:0"));
        Spi::run(
            "DELETE FROM tri_edges WHERE (src, dst) = (1, 3); SELECT warren.refresh('tri');
                  SELECT warren.triangles('tri', 'a', ARRAY['a'])",
        )?;
        assert_eq!(counts("a")?.as_deref(), Some("1:0 2:0 3:0 4:0 5:0"));
        Ok(())
    }

    #[pg_test]
    fn degrees_count_distinct_neighbours_like_warren_degree() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "SELECT warren.vector_degree('t', 'out');
             SELECT warren.vector_degree('t', 'in', 'in');
             SELECT warren.vector_degree('t', 'both', 'both', dtype := 'i64');
             SELECT warren.vector_degree('t', 'a', rels := ARRAY['a', 'nope'])",
        )?;
        assert_eq!(values("out")?.as_deref(), Some("1:2 2:1 3:1 4:1 5:1 6:0"));
        assert_eq!(values("in")?.as_deref(), Some("1:1 2:1 3:1 4:2 5:1 6:0"));
        assert_eq!(values("both")?.as_deref(), Some("1:2 2:2 3:2 4:2 5:1 6:0"));
        assert_eq!(values("a")?.as_deref(), Some("1:1 2:1 3:0 4:1 5:0 6:0"));
        let dtypes = rows("SELECT name || ':' || dtype FROM warren.vectors ORDER BY name")?;
        assert_eq!(dtypes.as_deref(), Some("a:f64 both:i64 in:f64 out:f64"));
        Ok(())
    }

    #[pg_test]
    fn dense_ids_number_the_id_space_and_tombstones_hold_zero() -> Result<(), Error> {
        small("t")?;
        Spi::run("DELETE FROM t_nodes WHERE id = 2")?;
        refresh("t", true)?;
        Spi::run("SELECT warren.vector_dense_id('t', 'id')")?;
        assert_eq!(values("id")?.as_deref(), Some("1:0 3:2 4:3 5:4 6:5"));
        let bytes = Spi::get_one::<Vec<u8>>(
            "SELECT data FROM warren.vector_chunks JOIN warren.vectors USING (vector_id)
             WHERE name = 'id'",
        )?
        .unwrap_or_default();
        assert_eq!(bytes[8..16], [0; 8], "the tombstoned row holds zero");
        assert_eq!(bytes[16], 2);
        Ok(())
    }
}
