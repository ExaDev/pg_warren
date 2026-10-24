//! `warren.spmv` and `warren.spmm` (DESIGN.md §6.3): `y = A ⊗ x` over a
//! semiring, with GraphBLAS masks, `replace` and accumulation.
//!
//! The product reads `y`'s old values only where it keeps them: outside a
//! mask without `replace`, or where `accumulate` combines them. Otherwise
//! (and when there is no `y` yet) `y` starts as `x`'s dtype and width
//! filled with the `⊕` identity, GraphBLAS's empty vector in dense form.

use pgrx::prelude::*;
use warren_core::compute::{
    self, map, AnyExpr, AnyVector, BinaryOp, Dtype, Expr, Mask, Monoid, Product, Semiring,
    SemiringElement, Vector,
};
use warren_core::graph::RelationSelection;

use super::vectors::Vectors;
use super::{entry, ComputeSqlError};
use crate::cache::{resolve, CachedIds};
use crate::interrupts::workers;
use crate::interrupts::PostgresInterrupts;
use crate::selection::relations;
use crate::source::with_id_type;

/// Which product a call computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// SpMV: width-1 vectors.
    Vector,
    /// SpMM: matrices of one width.
    Matrix,
}

/// The arguments of `spmv` and `spmm`.
struct Arguments<'a> {
    x: &'a str,
    y: &'a str,
    semiring: Semiring,
    transpose: bool,
    rels: Option<Vec<Option<String>>>,
    weighted: Option<bool>,
    mask: Option<&'a str>,
    complement: bool,
    replace: bool,
    accumulate: Option<BinaryOp>,
    persist: bool,
}

/// `vector` filled with `monoid`'s identity, at `vector`'s dtype and
/// shape.
fn identity<E: CachedIds>(
    vectors: &Vectors<E>,
    like: &AnyVector,
    monoid: Monoid,
) -> Result<AnyVector, ComputeSqlError> {
    fn filled<T: SemiringElement, E: CachedIds>(
        vectors: &Vectors<E>,
        width: usize,
        monoid: Monoid,
    ) -> Result<AnyVector, ComputeSqlError>
    where
        AnyVector: From<Vector<T>>,
    {
        let id_space = vectors.id_space();
        Ok(Vector::filled(&vectors.memory, id_space, width, T::identity(monoid))?.into())
    }
    let width = like.width();
    match like.dtype() {
        Dtype::F32 => filled::<f32, E>(vectors, width, monoid),
        Dtype::F64 => filled::<f64, E>(vectors, width, monoid),
        Dtype::I64 => filled::<i64, E>(vectors, width, monoid),
        Dtype::Bool => filled::<bool, E>(vectors, width, monoid),
    }
}

/// A copy of `vector`, charged to the call's budget, to write into.
fn copy<E: CachedIds>(
    vectors: &Vectors<E>,
    vector: &AnyVector,
) -> Result<AnyVector, ComputeSqlError> {
    let expr = match vector.dtype() {
        Dtype::F32 => AnyExpr::F32(Expr::Vector(0)),
        Dtype::F64 => AnyExpr::F64(Expr::Vector(0)),
        Dtype::I64 => AnyExpr::I64(Expr::Vector(0)),
        Dtype::Bool => AnyExpr::Bool(Expr::Vector(0)),
    };
    let id_space = vectors.id_space();
    Ok(map(&*workers()?, &vectors.memory, &mut PostgresInterrupts, id_space, &expr, &[vector])?)
}

impl Arguments<'_> {
    /// Computes the product into `y`.
    fn run<E: CachedIds>(
        &self,
        mut vectors: Vectors<E>,
        shape: Shape,
    ) -> Result<(), ComputeSqlError> {
        let x = vectors.get(self.x)?;
        let mask = self.mask.map(|name| vectors.get(name)).transpose()?;
        let keeps_y = (self.mask.is_some() && !self.replace) || self.accumulate.is_some();
        let old = match keeps_y {
            true => vectors.find(self.y)?,
            false => None,
        };
        let mut y = match old {
            Some(old) => copy(&vectors, &old)?,
            None => identity(&vectors, &x, self.semiring.add())?,
        };
        let loaded = &vectors.loaded;
        let selected = relations(&loaded.relations, self.rels.as_deref());
        let product = Product {
            semiring: self.semiring,
            transpose: self.transpose,
            relations: selected.as_deref().map_or(RelationSelection::All, RelationSelection::Only),
            weighted: self.weighted,
            per_relation: false,
            mask: mask.as_deref().map(|vector| Mask { vector, complement: self.complement }),
            replace: self.replace,
            accumulate: self.accumulate,
        };
        let multiply = match shape {
            Shape::Vector => compute::spmv,
            Shape::Matrix => compute::spmm,
        };
        let pool = workers()?;
        let graph = &loaded.graph;
        multiply(&pool, &vectors.memory, &mut PostgresInterrupts, graph, &product, &x, &mut y)?;
        vectors.save(self.y, y, self.persist)?;
        Ok(())
    }
}

/// Reads the arguments common to `spmv` and `spmm`, and runs the product.
#[allow(clippy::too_many_arguments)] // The SQL signature's arguments.
fn product(
    shape: Shape,
    graph: Option<&str>,
    x: Option<&str>,
    y: Option<&str>,
    semiring: Option<&str>,
    transpose: Option<bool>,
    rels: Option<Vec<Option<String>>>,
    weighted: Option<bool>,
    mask: Option<&str>,
    mask_complement: Option<bool>,
    replace: Option<bool>,
    accumulate: Option<&str>,
    persist: Option<bool>,
    version: Option<i64>,
) {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let arguments = Arguments {
            x: x.ok_or(ComputeSqlError::NullArgument("x"))?,
            y: y.ok_or(ComputeSqlError::NullArgument("y"))?,
            semiring: semiring.ok_or(ComputeSqlError::NullArgument("semiring"))?.parse()?,
            transpose: transpose.ok_or(ComputeSqlError::NullArgument("transpose"))?,
            rels,
            weighted,
            mask,
            complement: mask_complement.ok_or(ComputeSqlError::NullArgument("mask_complement"))?,
            replace: replace.ok_or(ComputeSqlError::NullArgument("replace"))?,
            accumulate: accumulate.map(str::parse).transpose()?,
            persist: persist.ok_or(ComputeSqlError::NullArgument("persist"))?,
        };
        let resolved = resolve(graph, version, "write vectors of")?;
        with_id_type!(resolved.graph.id_type, E => arguments.run(Vectors::<E>::open(resolved)?, shape))
    })
}

/// `warren.spmv`: `y = A ⊗ x` (`Aᵀ ⊗ x` if `transpose`) over `semiring`,
/// for width-1 vectors, written to vector `y` (transient if not `persist`).
#[allow(clippy::too_many_arguments)] // The SQL signature's arguments.
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn spmv(
    graph: Option<&str>,
    x: Option<&str>,
    y: Option<&str>,
    semiring: default!(Option<&str>, "'plus_times'"),
    transpose: default!(Option<bool>, "false"),
    rels: default!(Option<Vec<Option<String>>>, "NULL"),
    weighted: default!(Option<bool>, "NULL"),
    mask: default!(Option<&str>, "NULL"),
    mask_complement: default!(Option<bool>, "false"),
    replace: default!(Option<bool>, "false"),
    accumulate: default!(Option<&str>, "NULL"),
    persist: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
) {
    product(
        Shape::Vector,
        graph,
        x,
        y,
        semiring,
        transpose,
        rels,
        weighted,
        mask,
        mask_complement,
        replace,
        accumulate,
        persist,
        version,
    );
}

/// `warren.spmm`: `Y = A ⊗ X` (`Aᵀ ⊗ X` if `transpose`) over `semiring`,
/// for matrices of one width, written to matrix `y` (transient if not
/// `persist`); a mask selects rows.
#[allow(clippy::too_many_arguments)] // The SQL signature's arguments.
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn spmm(
    graph: Option<&str>,
    x: Option<&str>,
    y: Option<&str>,
    semiring: default!(Option<&str>, "'plus_times'"),
    transpose: default!(Option<bool>, "false"),
    rels: default!(Option<Vec<Option<String>>>, "NULL"),
    weighted: default!(Option<bool>, "NULL"),
    mask: default!(Option<&str>, "NULL"),
    mask_complement: default!(Option<bool>, "false"),
    replace: default!(Option<bool>, "false"),
    accumulate: default!(Option<&str>, "NULL"),
    persist: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
) {
    product(
        Shape::Matrix,
        graph,
        x,
        y,
        semiring,
        transpose,
        rels,
        weighted,
        mask,
        mask_complement,
        replace,
        accumulate,
        persist,
        version,
    );
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, rows, small};

    fn values(name: &str) -> Result<Option<String>, Error> {
        rows(&format!(
            "SELECT node_id || ':' || value FROM warren.vector_values('t', '{name}', NULL::bigint)"
        ))
    }

    /// x = node id at every node of the small graph.
    fn seeded() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT warren.vector_from_query('t', 'x', 'SELECT id, id FROM t_nodes')")
    }

    #[pg_test]
    fn spmv_combines_out_neighbours_and_transpose_in_neighbours() -> Result<(), Error> {
        seeded()?;
        // 1→2 (1), 2→3 (1), 3→4 (1), 1→4 (5), 4→1 (1), 5→5 (2).
        Spi::run("SELECT warren.spmv('t', 'x', 'y')")?;
        assert_eq!(values("y")?.as_deref(), Some("1:22 2:3 3:4 4:1 5:10 6:0"));
        Spi::run("SELECT warren.spmv('t', 'x', 'y', transpose := true)")?;
        assert_eq!(values("y")?.as_deref(), Some("1:4 2:1 3:2 4:8 5:10 6:0"));
        Spi::run("SELECT warren.spmv('t', 'x', 'y', 'min_plus', weighted := false)")?;
        assert_eq!(values("y")?.as_deref(), Some("1:3 2:4 3:5 4:2 5:6 6:Infinity"));
        Spi::run("SELECT warren.spmv('t', 'x', 'y', rels := ARRAY['a'])")?;
        assert_eq!(values("y")?.as_deref(), Some("1:2 2:3 3:0 4:1 5:0 6:0"));
        Ok(())
    }

    #[pg_test]
    fn masks_replace_and_accumulate_follow_graphblas() -> Result<(), Error> {
        seeded()?;
        Spi::run(
            "SELECT warren.vector_from_query('t', 'm', 'SELECT id, id % 2 FROM t_nodes');
             SELECT warren.vector_const('t', 'y', -1)",
        )?;
        Spi::run("SELECT warren.spmv('t', 'x', 'y', mask := 'm')")?;
        assert_eq!(values("y")?.as_deref(), Some("1:22 2:-1 3:4 4:-1 5:10 6:-1"));
        Spi::run("SELECT warren.spmv('t', 'x', 'y', mask := 'm', mask_complement := true, replace := true)")?;
        assert_eq!(values("y")?.as_deref(), Some("1:0 2:3 3:0 4:1 5:0 6:0"));
        Spi::run("SELECT warren.spmv('t', 'x', 'y', accumulate := 'plus')")?;
        assert_eq!(values("y")?.as_deref(), Some("1:22 2:6 3:4 4:2 5:10 6:0"));
        Spi::run("SELECT warren.spmv('t', 'x', 'fresh', mask := 'm')")?;
        assert_eq!(values("fresh")?.as_deref(), Some("1:22 2:0 3:4 4:0 5:10 6:0"));
        Ok(())
    }

    #[pg_test]
    fn spmm_aggregates_matrix_rows() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "SELECT warren.vector_from_query('t', 'e', 'SELECT id, ARRAY[id, 10 * id] FROM t_nodes', 'f32');
             SELECT warren.spmm('t', 'e', 'agg', weighted := false)",
        )?;
        let found = rows(
            "SELECT node_id || ':' || row::text FROM warren.matrix_rows('t', 'agg', NULL::bigint)",
        )?;
        assert_eq!(found.as_deref(), Some("1:{6,60} 2:{3,30} 3:{4,40} 4:{1,10} 5:{5,50} 6:{0,0}"));
        Ok(())
    }

    #[pg_test]
    fn bad_products_are_errors() -> Result<(), Error> {
        seeded()?;
        Spi::run("SELECT warren.vector_const('t', 'i', 1, 'i64')")?;
        let cases = [
            ("'x', 'y', 'plus_plus'", "22023: unknown semiring \"plus_plus\""),
            ("'x', 'y', accumulate := 'avg'", "22023: unknown binary operator \"avg\""),
            (
                "'x', 'i', accumulate := 'plus'",
                "42804: element type mismatch: expected i64, found f64",
            ),
            ("'nope', 'y'", "42704: graph \"t\" has no vector \"nope\""),
        ];
        for (args, expected) in cases {
            let found = failure(&format!("warren.spmv('t', {args})"))?.unwrap_or_default();
            assert!(found.starts_with(expected), "{args}: {found}");
        }
        Ok(())
    }
}
