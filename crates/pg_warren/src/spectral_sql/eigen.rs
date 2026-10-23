//! `warren.eigen` (DESIGN.md §6.5): the top-k eigenpairs of a graph
//! operator by Lanczos, the eigenvectors stored as vectors
//! `<vectors_prefix>_1..k` bound to the version.

use pgrx::prelude::*;
use warren_core::compute::Dtype;
use warren_core::spectral::{self, Eigen, EigenOptions, Precision, Resources};

use super::caveats::caveats;
use super::matrix::{solver_options, OperatorArguments};
use super::SpectralSqlError;
use crate::access::entry;
use crate::activity::Running;
use crate::cache::resolve;
use crate::compute_sql::{drop_named, vector_bytes, workers, Vectors};
use crate::interrupts::PostgresInterrupts;
use crate::source::{with_id_type, IdDatum};

/// One row of `eigen`'s result.
type Row = (i32, f64, f64);

/// Solves `operator` on `vectors`' version and stores the eigenvectors as
/// `<prefix>_1..k` if a prefix is given (transient unless `persist`),
/// dropping `<prefix>_<i>` for `i > k` left by an earlier, larger solve
/// first. Whether the caller may write (and drop) those vectors, and
/// whether transient eigenvectors fit `warren.max_cache_memory` (with the
/// transient ones they replace or drop released), is checked before
/// solving, so a refusal writes none.
fn solve<E: IdDatum>(
    mut vectors: Vectors<E>,
    operator: &OperatorArguments,
    options: &EigenOptions,
    prefix: Option<&str>,
    persist: bool,
) -> Result<Vec<Row>, SpectralSqlError> {
    let (names, stale) = match prefix {
        Some(prefix) => {
            let names: Vec<String> = (1..=options.k).map(|i| format!("{prefix}_{i}")).collect();
            let stale: Vec<String> = vectors
                .names()?
                .into_iter()
                .map(|bound| bound.name)
                .filter(|name| index(prefix, name).is_some_and(|i| i > options.k))
                .collect();
            (names, stale)
        }
        None => (Vec::new(), Vec::new()),
    };
    for name in &names {
        vectors.check_writable(name, persist)?;
    }
    for name in &stale {
        vectors.check_writable(name, false)?;
    }
    if !persist {
        let dtype = match options.precision {
            Precision::F32 => Dtype::F32,
            Precision::F64 => Dtype::F64,
        };
        vectors.admit_transients(&names, &stale, vector_bytes(dtype, 1, vectors.id_space()))?;
    }
    let eigen = eigenpairs(&vectors, operator, options)?;
    for warning in caveats(&eigen) {
        pgrx::warning!("{warning}");
    }
    for name in &stale {
        drop_named(&vectors.graph, vectors.version, name)?;
    }
    let mut rows = Vec::with_capacity(eigen.pairs.len());
    let mut names = names.into_iter();
    for (i, pair) in (1..).zip(eigen.pairs) {
        if let Some(name) = names.next() {
            vectors.save(&name, pair.vector, persist)?;
        }
        rows.push((i, pair.value, pair.residual));
    }
    Ok(rows)
}

/// `i` if `name` is `<prefix>_<i>`, `i` written as `format!` writes it.
fn index(prefix: &str, name: &str) -> Option<usize> {
    let i: usize = name.strip_prefix(prefix)?.strip_prefix('_')?.parse().ok()?;
    (name.len() == prefix.len() + 1 + i.to_string().len()).then_some(i)
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
    tol: default!(Option<f64>, "1e-6"),
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
        let _running =
            Running::start("eigen", Some(resolved.graph.graph_id), Some(resolved.version));
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

/// Refuses `eigenpairs(vectors, operator, options)` if its working set
/// cannot fit the compute memory, without solving.
pub(super) fn admit<E: IdDatum>(
    vectors: &Vectors<E>,
    operator: &OperatorArguments,
    options: &EigenOptions,
) -> Result<(), SpectralSqlError> {
    let loaded = &vectors.loaded;
    let selected = operator.relations(&loaded.relations);
    let matrix = operator.matrix(selected.as_deref())?;
    let pool = workers()?;
    let mut resources =
        Resources { workers: &pool, memory: &vectors.memory, interrupts: &mut PostgresInterrupts };
    Ok(spectral::admit_eigen(&mut resources, &loaded.graph, &matrix, options)?)
}

#[cfg(test)]
mod unit {
    use super::index;

    #[test]
    fn an_eigenvector_name_is_the_prefix_and_an_index() {
        assert_eq!(index("lap", "lap_12"), Some(12));
        assert_eq!(index("lap", "lap_012"), None);
        assert_eq!(index("lap", "lap_x"), None);
        assert_eq!(index("lap", "lap12"), None);
        assert_eq!(index("lap", "laplace_1"), None);
    }
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
    fn a_smaller_k_drops_the_eigenvectors_it_no_longer_writes() -> Result<(), Error> {
        path()?;
        Spi::run("SELECT * FROM warren.eigen('p', 3, vectors_prefix := 'lap')")?;
        Spi::run("SELECT warren.vector_const('p', 'lap_x', 1)")?;
        Spi::run("SELECT * FROM warren.eigen('p', 2, vectors_prefix := 'lap')")?;
        let names = rows("SELECT name FROM warren.vectors ORDER BY name")?;
        assert_eq!(names.as_deref(), Some("lap_1 lap_2 lap_x"));
        Ok(())
    }

    #[pg_test]
    fn vectors_it_may_not_replace_are_refused_before_solving() -> Result<(), Error> {
        // lap_1 belongs to the session user; a reader cannot replace it, and
        // is told so although the solve would not fit the memory cap.
        path()?;
        Spi::run(
            "SELECT * FROM warren.eigen('p', 1, vectors_prefix := 'lap');
             CREATE ROLE p_reader; SELECT warren.grant('p', 'p_reader');
             CREATE TEMP TABLE IF NOT EXISTS failure (what text); GRANT ALL ON failure TO p_reader;
             SET LOCAL warren.max_compute_memory = '64kB'; SET ROLE p_reader",
        )?;
        let found = failure("warren.eigen('p', 1, vectors_prefix := 'lap')")?.unwrap_or_default();
        Spi::run("RESET ROLE")?;
        assert!(found.starts_with("42501: "), "{found}");
        Ok(())
    }

    /// Graph `q`: the undirected path 1 – 2 – … – 2000, whose `f32`
    /// eigenvectors take 8,000 bytes each.
    fn long_path() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE q_nodes (id bigint); CREATE TABLE q_edges (src bigint, dst bigint);
             INSERT INTO q_nodes SELECT generate_series(1, 2000);
             INSERT INTO q_edges SELECT g, g + 1 FROM generate_series(1, 1999) g;
             SELECT warren.create_graph('q', 'SELECT id FROM q_nodes',
                                        'SELECT src, dst FROM q_edges', directed := false);
             SELECT warren.refresh('q')",
        )
    }

    /// A transient solve of `k` eigenvectors of `q` named `e_<i>`.
    fn transient_eigen(k: i32, dtype: &str) -> String {
        format!(
            "warren.eigen('q', {k}, tol := 1e-2, max_iter := 300, vectors_prefix := 'e',
                          dtype := '{dtype}', persist := false)"
        )
    }

    #[pg_test]
    fn transient_eigenvectors_over_the_cache_cap_are_refused_before_any_is_written(
    ) -> Result<(), Error> {
        long_path()?;
        Spi::run("SET LOCAL warren.max_cache_memory = '64kB'")?;
        for name in ["w", "x", "y"] {
            Spi::run(&format!("SELECT warren.vector_const('q', '{name}', 1, persist := false)"))?;
        }
        // 48,000 bytes held, and 3 × 8,000 more would pass 65,536.
        let refused = failure(&transient_eigen(3, "f32"))?.unwrap_or_default();
        assert!(refused.starts_with("53400: cache memory limit exceeded"), "{refused}");
        for name in ["e_1", "e_2", "e_3"] {
            let left = failure(&format!("warren.vector_values('q', '{name}', NULL::bigint)"))?;
            assert!(left.unwrap_or_default().starts_with("42704"), "{name} was written");
        }
        Ok(())
    }

    #[pg_test]
    fn the_eigenvectors_a_solve_replaces_or_drops_make_room() -> Result<(), Error> {
        long_path()?;
        Spi::run("SET LOCAL warren.max_cache_memory = '64kB'")?;
        Spi::run(&format!("SELECT * FROM {}", transient_eigen(2, "f32")))?;
        for name in ["w", "x", "y"] {
            Spi::run(&format!("SELECT warren.vector_const('q', '{name}', 1, persist := false)"))?;
        }
        // 64,000 bytes held; e_1 in f64 (16,000) fits only because it
        // replaces e_1 and drops e_2 (8,000 each).
        assert_eq!(failure(&transient_eigen(1, "f64"))?, None);
        let gone = failure("warren.vector_values('q', 'e_2', NULL::bigint)")?;
        assert!(gone.unwrap_or_default().starts_with("42704"));
        Ok(())
    }

    #[pg_test]
    fn a_solve_that_cannot_fit_is_refused_before_its_first_step() -> Result<(), Error> {
        // On q (2,000 rows, so 4,096 in the basis's blocks) k = 10 in f32
        // holds a basis of 21 vectors (344,064 bytes), the operator's
        // 66,000, H and its decomposition (14,280), the eigenvectors
        // (80,000) and the components pass's 16,000: 520,344 bytes. The
        // basis and the operator fit in 450kB, the eigenvectors do not;
        // the solve used to run its 100,000 steps before finding that out.
        long_path()?;
        Spi::run("SET LOCAL warren.max_compute_memory = '450kB'")?;
        let refused = failure("warren.eigen('q', 10, tol := 0, max_iter := 100000)")?;
        assert_eq!(
            refused.as_deref(),
            Some(
                "53400: compute memory limit exceeded: the eigenproblem needs 520344 bytes, \
                 0 of 460800 in use beside it"
            )
        );
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
