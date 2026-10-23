//! `warren.distance` (DESIGN.md §8): how far apart two versions of a graph
//! are, by one of five methods, as `jsonb` with several scalars (§14 item
//! 11). Versions of one graph are aligned by dense id and refused across a
//! compaction; with `graph2`, the second version is one of another graph,
//! aligned by external id.

mod methods;
mod pair;

use pgrx::prelude::*;
use pgrx::JsonB;
use serde_json::{json, Value};
use warren_core::spectral::ComparedRows;

use self::methods::Eigenvalues;
use self::pair::Pair;
use super::matrix::{solver_options, OperatorArguments};
use super::SpectralSqlError;
use crate::access::entry;
use crate::activity::Running;
use crate::cache::resolve;
use crate::source::{with_id_type, IdDatum};

/// What a `distance` call compares, checked.
enum Method<'a> {
    Edit,
    Degree,
    Spectral(OperatorArguments, Eigenvalues<'a>),
    Subspace { prefix: &'a str, k: usize, compared: ComparedRows },
    Embedding { x: &'a str, y: &'a str, compared: ComparedRows },
}

impl Method<'_> {
    fn name(&self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Degree => "degree",
            Self::Spectral(..) => "spectral",
            Self::Subspace { .. } => "subspace",
            Self::Embedding { .. } => "embedding",
        }
    }

    /// The distance between the versions of `pair`.
    fn measure<E: IdDatum>(&self, pair: &mut Pair<E>) -> Result<Value, SpectralSqlError> {
        match self {
            Self::Edit => methods::edit(pair),
            Self::Degree => methods::degree(pair),
            Self::Spectral(operator, eigenvalues) => methods::spectral(pair, operator, eigenvalues),
            Self::Subspace { prefix, k, compared } => {
                methods::subspace(pair, prefix, *k, *compared)
            }
            Self::Embedding { x, y, compared } => methods::embedding(pair, x, y, *compared),
        }
    }
}

/// `compared_rows`: `all` or `shared`.
fn parse_compared_rows(name: &str) -> Result<ComparedRows, SpectralSqlError> {
    match name {
        "all" => Ok(ComparedRows::All),
        "shared" => Ok(ComparedRows::Shared),
        other => Err(SpectralSqlError::InvalidArgument(format!(
            "unknown compared_rows \"{other}\": expected all or shared"
        ))),
    }
}

/// A positive `k`.
fn count(k: Option<i32>) -> Result<usize, SpectralSqlError> {
    let k = k.ok_or(SpectralSqlError::NullArgument("k"))?;
    usize::try_from(k)
        .ok()
        .filter(|&k| k > 0)
        .ok_or_else(|| SpectralSqlError::InvalidArgument(format!("k must be positive, not {k}")))
}

/// `warren.distance`: the `method` distance (edit, degree, spectral,
/// subspace, embedding) from version `v1` of graph `graph` to version `v2`
/// of graph `graph2` (default: the same graph), as `jsonb`.
///
/// - `edit` and `degree` need nothing more.
/// - `spectral` compares the eigenvalues of `<vectors_prefix>_1..k` under
///   `matrix`, which must then be given (Rayleigh quotients), or without a
///   prefix solves the `k` eigenpairs of `matrix` (default `adjacency`) at
///   the `which` end on each version.
/// - `subspace` compares the spans of `<vectors_prefix>_1..k`.
/// - `embedding` compares matrix `x` of `v1` with matrix `y` (default `x`)
///   of `v2`.
///
/// `subspace` and `embedding` read the rows of `compared_rows`: `shared`
/// nodes (present on both sides) or `all` (absent rows as zero).
#[allow(clippy::too_many_arguments)]
// The SQL signature's arguments.
// It reads and persists nothing (a fresh eigensolve for `spectral` is not
// stored), so it is a read: `PARALLEL RESTRICTED` (DESIGN.md §6.9).
#[pg_extern(schema = "warren", stable, parallel_restricted)]
fn distance(
    graph: Option<&str>,
    v1: Option<i64>,
    v2: Option<i64>,
    method: Option<&str>,
    vectors_prefix: default!(Option<&str>, "NULL"),
    k: default!(Option<i32>, "NULL"),
    x: default!(Option<&str>, "NULL"),
    y: default!(Option<&str>, "NULL"),
    compared_rows: default!(Option<&str>, "'shared'"),
    matrix: default!(Option<&str>, "NULL"),
    which: default!(Option<&str>, "'largest'"),
    rels: default!(Option<Vec<Option<String>>>, "NULL"),
    weighted: default!(Option<bool>, "NULL"),
    symmetrize: default!(Option<bool>, "true"),
    tol: default!(Option<f64>, "1e-6"),
    max_iter: default!(Option<i32>, "NULL"),
    dtype: default!(Option<&str>, "'f32'"),
    graph2: default!(Option<&str>, "NULL"),
) -> JsonB {
    entry(|_| {
        let graph = graph.ok_or(SpectralSqlError::NullArgument("graph"))?;
        let v1 = v1.ok_or(SpectralSqlError::NullArgument("v1"))?;
        let v2 = v2.ok_or(SpectralSqlError::NullArgument("v2"))?;
        let compared = || {
            parse_compared_rows(
                compared_rows.ok_or(SpectralSqlError::NullArgument("compared_rows"))?,
            )
        };
        let prefix = || vectors_prefix.ok_or(SpectralSqlError::NullArgument("vectors_prefix"));
        let method = match method.ok_or(SpectralSqlError::NullArgument("method"))? {
            "edit" => Method::Edit,
            "degree" => Method::Degree,
            "spectral" => {
                // Stored eigenvectors carry no record of their operator, so
                // their Rayleigh quotients need it named; a fresh solve
                // defaults to the adjacency, as eigen does.
                let matrix = match (matrix, vectors_prefix) {
                    (Some(matrix), _) => matrix,
                    (None, None) => "adjacency",
                    (None, Some(_)) => {
                        return Err(SpectralSqlError::InvalidArgument(
                            "spectral with vectors_prefix needs the matrix the eigenvectors are of"
                                .to_owned(),
                        ))
                    }
                };
                let operator = OperatorArguments {
                    matrix: matrix.to_owned(),
                    rels,
                    weighted,
                    symmetrize: symmetrize.ok_or(SpectralSqlError::NullArgument("symmetrize"))?,
                };
                let eigenvalues = match vectors_prefix {
                    Some(prefix) => Eigenvalues::Stored { prefix, k: count(k)? },
                    None => Eigenvalues::Solved(solver_options(
                        k,
                        which.ok_or(SpectralSqlError::NullArgument("which"))?,
                        tol.ok_or(SpectralSqlError::NullArgument("tol"))?,
                        max_iter,
                        dtype.ok_or(SpectralSqlError::NullArgument("dtype"))?,
                    )?),
                };
                Method::Spectral(operator, eigenvalues)
            }
            "subspace" => Method::Subspace { prefix: prefix()?, k: count(k)?, compared: compared()? },
            "embedding" => {
                let x = x.ok_or(SpectralSqlError::NullArgument("x"))?;
                Method::Embedding { x, y: y.unwrap_or(x), compared: compared()? }
            }
            other => {
                return Err(SpectralSqlError::InvalidArgument(format!(
                    "unknown method \"{other}\": expected edit, degree, spectral, subspace or embedding"
                )))
            }
        };
        let before = resolve(graph, Some(v1), "read")?;
        let after = resolve(graph2.unwrap_or(graph), Some(v2), "read")?;
        let _running = Running::start("distance", Some(before.graph.graph_id), Some(v1));
        let mut result = with_id_type!(before.graph.id_type, E => {
            method.measure(&mut Pair::<E>::open(before, after)?)?
        });
        result["method"] = json!(method.name());
        result["from"] = json!(v1);
        result["to"] = json!(v2);
        if let Some(graph2) = graph2 {
            result["graph2"] = json!(graph2);
        }
        Ok::<_, SpectralSqlError>(JsonB(result))
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::failure;

    /// Graph `g` over `g_nodes`/`g_edges`, undirected, with relations: two
    /// triangles joined by an edge. Its first version is returned.
    fn graph() -> Result<i64, Error> {
        Spi::run(
            "CREATE TABLE g_nodes (id bigint); CREATE TABLE g_edges (src bigint, dst bigint, rel text);
             INSERT INTO g_nodes SELECT generate_series(1, 6);
             INSERT INTO g_edges VALUES (1, 2, 'a'), (2, 3, 'a'), (3, 1, 'a'),
                                        (4, 5, 'b'), (5, 6, 'b'), (6, 4, 'b'), (3, 4, 'a');
             SELECT warren.create_graph('g', 'SELECT id FROM g_nodes', 'SELECT src, dst, rel FROM g_edges',
                                        directed := false, has_relations := true)",
        )?;
        refreshed("g")
    }

    fn refreshed(graph: &str) -> Result<i64, Error> {
        Spi::get_one::<i64>(&format!("SELECT warren.refresh('{graph}')"))?
            .ok_or(Error::InvalidPosition)
    }

    /// Stores eigenvectors `lap_1..3` and embedding `e` on `graph`'s version.
    fn analysed(graph: &str, version: i64) -> Result<(), Error> {
        Spi::run(&format!(
            "SELECT * FROM warren.eigen('{graph}', 3, 'normalized_laplacian', 'smallest', dtype := 'f64',
                                        tol := 1e-12, vectors_prefix := 'lap', version := {version});
             SELECT warren.vector_from_query('{graph}', 'e', 'SELECT id, ARRAY[id, id * id] FROM {graph}_nodes',
                                             version := {version})"
        ))
    }

    fn distance(args: &str) -> Result<serde_json::Value, Error> {
        let json = Spi::get_one::<pgrx::JsonB>(&format!("SELECT warren.distance({args})"))?;
        Ok(json.ok_or(Error::InvalidPosition)?.0)
    }

    const METHODS: [&str; 5] = [
        "'edit'",
        "'degree'",
        "'spectral', k := 2, dtype := 'f64', tol := 1e-12",
        "'subspace', vectors_prefix := 'lap', k := 3",
        "'embedding', x := 'e'",
    ];

    #[pg_test]
    fn a_version_is_zero_from_itself_by_every_method() -> Result<(), Error> {
        let v = graph()?;
        analysed("g", v)?;
        let zero = |json: &serde_json::Value, key: &str| json[key].as_f64() == Some(0.0);
        let d = distance(&format!("'g', {v}, {v}, 'edit'"))?;
        assert!(zero(&d["overall"], "distance"), "{d}");
        assert_eq!(d["relations"][0]["relation"], "a");
        assert_eq!((d["method"].as_str(), d["from"].as_i64()), (Some("edit"), Some(v)));
        let d = distance(&format!("'g', {v}, {v}, 'degree'"))?;
        assert!(zero(&d, "wasserstein") && zero(&d, "ks"), "{d}");
        let d = distance(&format!("'g', {v}, {v}, 'spectral', k := 2"))?;
        assert!(zero(&d, "l2"), "{d}");
        assert_eq!(d["converged"], true);
        let d = distance(&format!("'g', {v}, {v}, 'spectral', vectors_prefix := 'lap', k := 3, matrix := 'normalized_laplacian'"))?;
        assert!(d["l2"].as_f64().is_some_and(|l2| l2 == 0.0), "{d}");
        assert!(d["before"][0].as_f64().is_some_and(|value| value.abs() < 1e-9), "{d}");
        let d = distance(&format!("'g', {v}, {v}, 'subspace', vectors_prefix := 'lap', k := 3"))?;
        assert!(zero(&d, "projection") && zero(&d, "max_angle"), "{d}");
        let d = distance(&format!("'g', {v}, {v}, 'embedding', x := 'e'"))?;
        assert!(zero(&d, "mean_l2") && d["rows"].as_u64() == Some(6), "{d}");
        Ok(())
    }

    #[pg_test]
    fn a_changed_version_is_measured_and_a_compaction_is_a_boundary() -> Result<(), Error> {
        let v1 = graph()?;
        analysed("g", v1)?;
        Spi::run(
            "DELETE FROM g_edges WHERE src = 3 AND dst = 4; INSERT INTO g_nodes VALUES (7);
                  INSERT INTO g_edges VALUES (6, 7, 'c')",
        )?;
        let v2 = refreshed("g")?;
        analysed("g", v2)?;
        let d = distance(&format!("'g', {v1}, {v2}, 'edit'"))?;
        // Undirected: each edge counts in both directions.
        assert_eq!(d["overall"]["added"], 2);
        assert_eq!(d["overall"]["removed"], 2);
        assert_eq!(d["relations"][2]["relation"], "c");
        let d = distance(&format!("'g', {v1}, {v2}, 'subspace', vectors_prefix := 'lap', k := 3"))?;
        assert!(d["projection"].as_f64().is_some_and(|p| p > 0.1), "{d}");
        let d = distance(&format!("'g', {v1}, {v2}, 'degree'"))?;
        assert!(d["wasserstein"].as_f64().is_some_and(|w| w > 0.0), "{d}");
        Spi::run("SELECT warren.compact('g')")?;
        let v3 = Spi::get_one::<i64>("SELECT current_version FROM warren.graphs WHERE name = 'g'")?
            .ok_or(Error::InvalidPosition)?;
        for method in METHODS {
            let found = failure(&format!("warren.distance('g', {v1}, {v3}, {method})"))?
                .unwrap_or_default();
            assert!(
                found.starts_with("22023: versions") && found.contains("different id spaces"),
                "{found}"
            );
        }
        Ok(())
    }

    #[pg_test]
    fn a_fresh_solve_that_stops_early_says_so() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE q_nodes (id bigint); CREATE TABLE q_edges (src bigint, dst bigint);
             INSERT INTO q_nodes SELECT generate_series(1, 200);
             INSERT INTO q_edges SELECT g, (g * 7) % 200 + 1 FROM generate_series(1, 200) g
                       UNION ALL SELECT g, g % 200 + 1 FROM generate_series(1, 200) g;
             SELECT warren.create_graph('q', 'SELECT id FROM q_nodes', 'SELECT src, dst FROM q_edges',
                                        directed := false)",
        )?;
        let v = refreshed("q")?;
        let d = distance(&format!("'q', {v}, {v}, 'spectral', k := 4, max_iter := 25"))?;
        assert_eq!(d["converged"], false, "{d}");
        Ok(())
    }

    #[pg_test]
    fn a_fresh_solve_of_either_version_that_cannot_fit_is_refused_before_either_runs(
    ) -> Result<(), Error> {
        // The path 1 – … – 2,000 and then 1 – … – 10,000: k = 2 in f32 on the
        // first holds 438,640 bytes, on the second 1,486,000, so in 1000kB
        // only the first fits. It used to solve the first (here 100,000
        // steps) before the second was refused.
        Spi::run(
            "CREATE TABLE r_nodes (id bigint); CREATE TABLE r_edges (src bigint, dst bigint);
             INSERT INTO r_nodes SELECT generate_series(1, 2000);
             INSERT INTO r_edges SELECT g, g + 1 FROM generate_series(1, 1999) g;
             SELECT warren.create_graph('r', 'SELECT id FROM r_nodes', 'SELECT src, dst FROM r_edges',
                                        directed := false)",
        )?;
        let first = refreshed("r")?;
        Spi::run(
            "INSERT INTO r_nodes SELECT generate_series(2001, 10000);
             INSERT INTO r_edges SELECT g, g + 1 FROM generate_series(2000, 9999) g",
        )?;
        let second = refreshed("r")?;
        Spi::run("SET LOCAL warren.max_compute_memory = '1000kB'")?;
        let call = format!(
            "warren.distance('r', {first}, {second}, 'spectral', k := 2, tol := 0, max_iter := 100000)"
        );
        let started = std::time::Instant::now();
        let refused = failure(&call)?.unwrap_or_default();
        let expected = "53400: compute memory limit exceeded: the eigenproblem needs 1486000 bytes";
        assert!(refused.starts_with(expected), "{refused}");
        assert!(started.elapsed().as_secs_f64() < 2.0, "{:?}", started.elapsed());
        Ok(())
    }

    #[pg_test]
    fn sibling_staged_versions_are_refused() -> Result<(), Error> {
        // Two staged builds from one current version: node 10 in one and node
        // 20 in the other get the same new dense id, relations x and y the
        // same relation index. Like diff, distance refuses to compare them.
        graph()?;
        Spi::run("INSERT INTO g_nodes VALUES (10); INSERT INTO g_edges VALUES (4, 10, 'x')")?;
        let with_10 = staged("g")?;
        Spi::run(
            "DELETE FROM g_edges WHERE dst = 10; DELETE FROM g_nodes WHERE id = 10;
             INSERT INTO g_nodes VALUES (20); INSERT INTO g_edges VALUES (1, 20, 'y')",
        )?;
        let with_20 = staged("g")?;
        for version in [with_10, with_20] {
            Spi::run(&format!(
                "SELECT warren.vector_from_query('g', 'e', 'SELECT id, ARRAY[id::float8] FROM g_nodes',
                                                 version := {version})"
            ))?;
        }
        let diff = failure(&format!("warren.diff('g', {with_10}, {with_20})"))?.unwrap_or_default();
        assert!(diff.starts_with("22023: dense id"), "{diff}");
        for method in ["'edit'", "'degree'", "'spectral', k := 1", "'embedding', x := 'e'"] {
            let found = failure(&format!("warren.distance('g', {with_10}, {with_20}, {method})"))?
                .unwrap_or_default();
            assert!(
                found.starts_with("22023: dense id 6 names different nodes"),
                "{method}: {found}"
            );
        }
        Ok(())
    }

    fn staged(graph: &str) -> Result<i64, Error> {
        Spi::get_one::<i64>(&format!("SELECT warren.refresh('{graph}', promote := false)"))?
            .ok_or(Error::InvalidPosition)
    }

    #[pg_test]
    fn another_graph_is_compared_by_external_id() -> Result<(), Error> {
        // `h` holds g's later version, built at once from tables where node 0
        // comes first: every dense id differs from g's.
        let v1 = graph()?;
        analysed("g", v1)?;
        Spi::run(
            "INSERT INTO g_nodes VALUES (0); INSERT INTO g_edges VALUES (0, 1, 'c'), (0, 5, 'c')",
        )?;
        let v2 = refreshed("g")?;
        analysed("g", v2)?;
        Spi::run(
            "CREATE TABLE h_nodes AS SELECT * FROM g_nodes; CREATE TABLE h_edges AS SELECT * FROM g_edges;
             SELECT warren.create_graph('h', 'SELECT id FROM h_nodes', 'SELECT src, dst, rel FROM h_edges',
                                        directed := false, has_relations := true)",
        )?;
        let h = refreshed("h")?;
        analysed("h", h)?;
        for method in METHODS {
            let same = distance(&format!("'g', {v1}, {v2}, {method}"))?;
            let cross = distance(&format!("'g', {v1}, {h}, {method}, graph2 := 'h'"))?;
            assert_eq!(cross["graph2"], "h");
            close(&same, &cross, method);
        }
        Ok(())
    }

    /// Whether every number in `a` is within 1e-9 of the same one in `b`.
    fn close(a: &serde_json::Value, b: &serde_json::Value, what: &str) {
        use serde_json::Value;
        match (a, b) {
            (Value::Number(x), Value::Number(y)) => {
                let (x, y) = (x.as_f64().unwrap_or(f64::NAN), y.as_f64().unwrap_or(f64::NAN));
                assert!((x - y).abs() <= 1e-9, "{what}: {x} vs {y}");
            }
            (Value::Array(x), Value::Array(y)) => {
                assert_eq!(x.len(), y.len(), "{what}");
                x.iter().zip(y).for_each(|(x, y)| close(x, y, what));
            }
            (Value::Object(x), Value::Object(y)) => {
                for (key, x) in
                    x.iter().filter(|(key, _)| !["to", "graph2"].contains(&key.as_str()))
                {
                    close(x, &y[key], &format!("{what}.{key}"));
                }
            }
            (x, y) => assert_eq!(x, y, "{what}"),
        }
    }

    #[pg_test]
    fn bad_distances_are_errors() -> Result<(), Error> {
        let v = graph()?;
        Spi::run(
            "CREATE TABLE u_nodes (id uuid); SELECT warren.create_graph('u', 'SELECT id FROM u_nodes',
                                                                 'SELECT id, id FROM u_nodes')",
        )?;
        let u = refreshed("u")?;
        let cases = [
            (format!("'g', {v}, {v}, 'cosine'"), "22023: unknown method \"cosine\""),
            (
                format!("'g', {v}, {v}, 'spectral', vectors_prefix := 'lap', k := 1"),
                "22023: spectral with vectors_prefix needs the matrix",
            ),
            (
                format!("'g', {v}, {v}, 'subspace', k := 2"),
                "22004: vectors_prefix must not be NULL",
            ),
            (
                format!("'g', {v}, {v}, 'subspace', vectors_prefix := 'lap'"),
                "22004: k must not be NULL",
            ),
            (format!("'g', {v}, {v}, 'embedding'"), "22004: x must not be NULL"),
            (
                format!("'g', {v}, {v}, 'embedding', x := 'e', compared_rows := 'some'"),
                "22023: unknown compared_rows",
            ),
            (
                format!("'g', {v}, {v}, 'subspace', vectors_prefix := 'none', k := 1"),
                "42704: graph \"g\" has no vector \"none_1\"",
            ),
            (
                format!("'g', {v}, {u}, 'edit', graph2 := 'u'"),
                "42804: graphs \"g\" and \"u\" have different id types",
            ),
            (format!("'g', {v}, NULL, 'edit'"), "22004: v2 must not be NULL"),
        ];
        for (args, expected) in cases {
            let found = failure(&format!("warren.distance({args})"))?.unwrap_or_default();
            assert!(found.starts_with(expected), "{args}: {found}");
        }
        Ok(())
    }
}
