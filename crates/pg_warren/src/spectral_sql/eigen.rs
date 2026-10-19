//! `warren.eigen` (DESIGN.md §6.5): the top-k eigenpairs of a graph
//! operator by Lanczos, the eigenvectors stored as vectors
//! `<vectors_prefix>_1..k` bound to the version.

use pgrx::prelude::*;
use warren_core::spectral::{self, Eigen, EigenOptions, Resources};

use super::matrix::{solver_options, OperatorArguments};
use super::SpectralSqlError;
use crate::access::entry;
use crate::cache::resolve;
use crate::compute_sql::{workers, Vectors};
use crate::interrupts::PostgresInterrupts;
use crate::source::{with_id_type, IdDatum};

/// One row of `eigen`'s result.
type Row = (i32, f64, f64);

/// Solves `operator` on `vectors`' version and stores the eigenvectors as
/// `<prefix>_1..k` if a prefix is given (transient unless `persist`).
fn solve<E: IdDatum>(
    mut vectors: Vectors<E>,
    operator: &OperatorArguments,
    options: &EigenOptions,
    prefix: Option<&str>,
    persist: bool,
) -> Result<Vec<Row>, SpectralSqlError> {
    let eigen = eigenpairs(&vectors, operator, options)?;
    let mut rows = Vec::with_capacity(eigen.pairs.len());
    for (i, pair) in (1..).zip(eigen.pairs) {
        if let Some(prefix) = prefix {
            vectors.save(&format!("{prefix}_{i}"), pair.vector, persist)?;
        }
        rows.push((i, pair.value, pair.residual));
    }
    Ok(rows)
}

/// `warren.eigen`: the `k` eigenpairs of `matrix` (adjacency, laplacian,
/// normalized_laplacian, random_walk) of graph `graph` at the `which` end
/// (largest magnitude, or smallest), as `(i, eigenvalue, residual)` rows.
/// With `vectors_prefix`, eigenvector `i` is stored as vector
/// `<vectors_prefix>_<i>` (transient if not `persist`).
#[allow(clippy::too_many_arguments)] // The SQL signature's arguments.
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn eigen(
    graph: Option<&str>,
    k: Option<i32>,
    matrix: default!(Option<&str>, "'adjacency'"),
    which: default!(Option<&str>, "'largest'"),
    rels: default!(Option<Vec<Option<String>>>, "NULL"),
    weighted: default!(Option<bool>, "NULL"),
    symmetrize: default!(Option<bool>, "true"),
    tol: default!(Option<f64>, "1e-8"),
    max_iter: default!(Option<i32>, "NULL"),
    vectors_prefix: default!(Option<&str>, "NULL"),
    version: default!(Option<i64>, "NULL"),
    dtype: default!(Option<&str>, "'f32'"),
    persist: default!(Option<bool>, "true"),
) -> TableIterator<'static, (name!(i, i32), name!(eigenvalue, f64), name!(residual, f64))> {
    entry(|_| {
        let graph = graph.ok_or(SpectralSqlError::NullArgument("graph"))?;
        let operator = OperatorArguments {
            matrix: matrix.ok_or(SpectralSqlError::NullArgument("matrix"))?.to_owned(),
            rels,
            weighted,
            symmetrize: symmetrize.ok_or(SpectralSqlError::NullArgument("symmetrize"))?,
        };
        let options = solver_options(
            k,
            which.ok_or(SpectralSqlError::NullArgument("which"))?,
            tol.ok_or(SpectralSqlError::NullArgument("tol"))?,
            max_iter,
            dtype.ok_or(SpectralSqlError::NullArgument("dtype"))?,
        )?;
        let persist = persist.ok_or(SpectralSqlError::NullArgument("persist"))?;
        let action = if vectors_prefix.is_some() { "write vectors of" } else { "read" };
        let resolved = resolve(graph, version, action)?;
        let rows = with_id_type!(resolved.graph.id_type, E => {
            solve(Vectors::<E>::open(resolved)?, &operator, &options, vectors_prefix, persist)?
        });
        Ok::<_, SpectralSqlError>(TableIterator::new(rows))
    })
}

/// The eigenpairs of `operator` over `vectors`' version.
pub(super) fn eigenpairs<E: IdDatum>(
    vectors: &Vectors<E>,
    operator: &OperatorArguments,
    options: &EigenOptions,
) -> Result<Eigen, SpectralSqlError> {
    let loaded = &vectors.loaded;
    let selected = operator.relations(&loaded.relations);
    let matrix = operator.matrix(selected.as_deref())?;
    let pool = workers()?;
    let mut resources =
        Resources { workers: &pool, memory: &vectors.memory, interrupts: &mut PostgresInterrupts };
    Ok(spectral::eigen(&mut resources, &loaded.graph, &matrix, options)?)
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, rows};

    /// Graph `p` over `p_nodes`/`p_edges`: the undirected path 1 – 2 – … – 6
    /// with unit weights.
    fn path() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE p_nodes (id bigint); CREATE TABLE p_edges (src bigint, dst bigint);
             INSERT INTO p_nodes SELECT generate_series(1, 6);
             INSERT INTO p_edges SELECT g, g + 1 FROM generate_series(1, 5) g;
             SELECT warren.create_graph('p', 'SELECT id FROM p_nodes',
                                        'SELECT src, dst FROM p_edges', directed := false);
             SELECT warren.refresh('p')",
        )
    }

    fn eigenvalues(call: &str) -> Result<Option<String>, Error> {
        rows(&format!("SELECT i || ':' || round(eigenvalue::numeric, 6) FROM {call} ORDER BY i"))
    }

    #[pg_test]
    fn the_laplacian_spectrum_of_a_path() -> Result<(), Error> {
        // L of P_6 has eigenvalues 2 − 2cos(πj/6): 0, 0.267949, 1, 2, 3, 3.732051.
        path()?;
        let call = "warren.eigen('p', 3, 'laplacian', 'smallest', dtype := 'f64')";
        assert_eq!(eigenvalues(call)?.as_deref(), Some("1:0.000000 2:0.267949 3:1.000000"));
        let residuals = Spi::get_one::<f64>(&format!("SELECT max(residual) FROM {call}"))?;
        assert!(residuals.is_some_and(|r| r < 1e-8), "{residuals:?}");
        let largest = "warren.eigen('p', 2, 'laplacian', symmetrize := false)";
        assert_eq!(eigenvalues(largest)?.as_deref(), Some("1:3.732051 2:3.000000"));
        Ok(())
    }

    #[pg_test]
    fn eigenvectors_are_stored_as_unit_vectors() -> Result<(), Error> {
        path()?;
        Spi::run("SELECT * FROM warren.eigen('p', 2, 'normalized_laplacian', 'smallest', vectors_prefix := 'lap')")?;
        let stored = rows("SELECT name || ':' || dtype FROM warren.vectors ORDER BY name")?;
        assert_eq!(stored.as_deref(), Some("lap_1:f32 lap_2:f32"));
        let norms = rows(
            "SELECT round(warren.vector_reduce('p', n, 'l2')::numeric, 5)::text
             FROM unnest(ARRAY['lap_1', 'lap_2']) n",
        )?;
        assert_eq!(norms.as_deref(), Some("1.00000 1.00000"));
        Spi::run("SELECT * FROM warren.eigen('p', 1, vectors_prefix := 'tmp', persist := false, dtype := 'f64')")?;
        let persisted =
            Spi::get_one::<i64>("SELECT count(*) FROM warren.vectors WHERE name = 'tmp_1'")?;
        assert_eq!(persisted, Some(0));
        let transient = Spi::get_one::<f64>("SELECT warren.vector_reduce('p', 'tmp_1', 'l2')")?;
        assert!(transient.is_some_and(|norm| (norm - 1.0).abs() < 1e-12));
        Ok(())
    }

    #[pg_test]
    fn results_are_the_same_for_every_run_and_thread_count() -> Result<(), Error> {
        path()?;
        let call = "SELECT string_agg(eigenvalue::text || '/' || residual::text, ' ' ORDER BY i)
                    FROM warren.eigen('p', 3, 'random_walk', vectors_prefix := 'rw')";
        let values = |name: &str| {
            rows(&format!(
                "SELECT value::text FROM warren.vector_values('p', '{name}', NULL::bigint)"
            ))
        };
        Spi::run("SET LOCAL warren.threads = 1")?;
        let one = (Spi::get_one::<String>(call)?, values("rw_1")?);
        Spi::run("SET LOCAL warren.threads = 3")?;
        let three = (Spi::get_one::<String>(call)?, values("rw_1")?);
        assert_eq!(one, three);
        assert!(one.0.is_some());
        Ok(())
    }

    #[pg_test]
    fn bad_eigenproblems_are_errors() -> Result<(), Error> {
        path()?;
        Spi::run(
            "CREATE TABLE d_nodes (id bigint); CREATE TABLE d_edges (src bigint, dst bigint);
             INSERT INTO d_nodes VALUES (1), (2), (3); INSERT INTO d_edges VALUES (1, 2), (2, 3);
             SELECT warren.create_graph('d', 'SELECT id FROM d_nodes', 'SELECT src, dst FROM d_edges');
             SELECT warren.refresh('d')",
        )?;
        let cases = [
            ("'p', NULL", "22004: k must not be NULL"),
            ("'p', 0", "22023: k must be positive, not 0"),
            ("'p', 7", "22023: cannot compute 7 eigenpairs of a matrix of dimension 6"),
            ("'p', 1, 'svd'", "22023: unknown matrix \"svd\""),
            ("'p', 1, which := 'middle'", "22023: unknown which \"middle\""),
            ("'p', 1, dtype := 'i64'", "22023: unknown precision \"i64\""),
            ("'d', 1, symmetrize := false", "22023: the matrix is not symmetric"),
            ("'nope', 1", "42704: graph \"nope\" does not exist"),
            // The library's reserved prefix is never persisted (§6.7).
            ("'p', 1, vectors_prefix := '_warren_e'", "42939: "),
        ];
        for (args, expected) in cases {
            let found = failure(&format!("warren.eigen({args})"))?.unwrap_or_default();
            assert!(found.starts_with(expected), "{args}: {found}");
        }
        Spi::run("SET LOCAL warren.max_compute_memory = '64kB'")?;
        Spi::run(
            "CREATE TABLE b_nodes (id bigint); INSERT INTO b_nodes SELECT generate_series(1, 2000);
             SELECT warren.create_graph('b', 'SELECT id FROM b_nodes',
                                        'SELECT id, id % 2000 + 1 FROM b_nodes', directed := false);
             SELECT warren.refresh('b')",
        )?;
        let found = failure("warren.eigen('b', 2)")?.unwrap_or_default();
        assert!(found.starts_with("53400: compute memory limit exceeded"), "{found}");
        Ok(())
    }
}
