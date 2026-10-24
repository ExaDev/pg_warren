//! Structural kernels of the algorithm library (DESIGN.md §6.7, planned
//! library group 2): `core_number`, `strong_components_vector`,
//! `topological_order`, `dag_longest_path` and
//! `minimum_spanning_forest`. Each runs a `warren_core::compute` kernel
//! on the compute pool within the call's memory budget; all but the last
//! write a named vector.

use pgrx::prelude::*;
use pgrx::AnyElement;
use warren_core::compute;

use super::vectors::Vectors;
use super::{entry, selection, ComputeSqlError};
use crate::cache::{cached, compute_memory, resolve, CachedIds, Resolved};
use crate::external_ids::{check_type, external};
use crate::interrupts::workers;
use crate::interrupts::PostgresInterrupts;
use crate::selection::relations;
use crate::source::with_id_type;

/// `warren.core_number` (§6.7): vector `out` (`i64`) holding each node's
/// core number in the undirected simple graph under the relations `rels`
/// (all if NULL), edge directions and self-loops ignored, as NetworkX's
/// `core_number`. Returns the graph's degeneracy, the largest core number
/// (0 without edges).
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn core_number(
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
            let found = compute::core_number(
                &*workers()?,
                &vectors.memory,
                &mut PostgresInterrupts,
                &loaded.graph,
                selection(selected.as_deref()),
            )?;
            vectors.save(out, found.cores.into(), persist)?;
            Ok(Some(found.degeneracy))
        })
    })
}

/// `warren.strong_components_vector` (§6.7): vector `out` (`i64`) labelling
/// each node with the smallest dense id of its strongly connected
/// component over the relations `rels` (all if NULL), as
/// `components_vector` labels weak components. Returns the number of
/// strong components among the live nodes.
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn strong_components_vector(
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
            let labels = compute::strong_components(
                &*workers()?,
                &vectors.memory,
                &mut PostgresInterrupts,
                &loaded.graph,
                selection(selected.as_deref()),
            )?;
            let tombstones = vectors.tombstones();
            let roots = (0..).zip(labels.values()).filter(|&(node, &label)| {
                label == node as i64 && !tombstones.contains(node)
            });
            let count = roots.count() as i64;
            vectors.save(out, labels.into(), persist)?;
            Ok(Some(count))
        })
    })
}

/// `warren.topological_order` (§6.7): vector `out` (`i64`) holding each
/// node's position, from 0, in the topological order of the relations
/// `rels` (all if NULL): among the nodes ready at once, the smallest dense
/// id first (Kahn's algorithm). A cycle is an error (22023). Returns its
/// vector id (NULL if not `persist`).
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn topological_order(
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
            let positions = compute::topological_order(
                &*workers()?,
                &vectors.memory,
                &mut PostgresInterrupts,
                &loaded.graph,
                selection(selected.as_deref()),
                vectors.tombstones(),
            )?;
            vectors.save(out, positions.into(), persist)
        })
    })
}

/// `warren.dag_longest_path` (§6.7): vector `out` (`f64`) holding the
/// length of the longest path ending at each node over the relations
/// `rels` (all if NULL): the sum of its edges' weights if `weighted` (NULL:
/// if the graph has weights), else its number of edges, and at least 0,
/// as NetworkX's `dag_longest_path`. A cycle is an error (22023). Returns
/// the longest, NetworkX's `dag_longest_path_length`.
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn dag_longest_path(
    graph: Option<&str>,
    out: Option<&str>,
    rels: default!(Option<Vec<Option<String>>>, "NULL"),
    weighted: default!(Option<bool>, "NULL"),
    persist: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
) -> Option<f64> {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let out = out.ok_or(ComputeSqlError::NullArgument("out"))?;
        let persist = persist.ok_or(ComputeSqlError::NullArgument("persist"))?;
        let resolved = resolve(graph, version, "write vectors of")?;
        with_id_type!(resolved.graph.id_type, E => {
            let mut vectors = Vectors::<E>::open(resolved)?;
            let loaded = &vectors.loaded;
            let selected = relations(&loaded.relations, rels.as_deref());
            let found = compute::dag_longest_path(
                &*workers()?,
                &vectors.memory,
                &mut PostgresInterrupts,
                &loaded.graph,
                selection(selected.as_deref()),
                weighted,
                vectors.tombstones(),
            )?;
            vectors.save(out, found.lengths.into(), persist)?;
            Ok(Some(found.longest))
        })
    })
}

/// `warren.minimum_spanning_forest` (§6.7): the edges of a minimum
/// spanning tree of every component of the undirected graph under the
/// relations `rels` (all if NULL), self-loops ignored, each as its stored
/// direction with its weight (1 if not `weighted`; NULL: if the graph has
/// weights), ascending by source and target dense id. `id_type` fixes the
/// id columns' type, as `warren.components`'s does.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.minimum_spanning_forest(graph text, id_type anyelement, rels text[] DEFAULT NULL,
                                               weighted bool DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS TABLE (source anyelement, target anyelement, weight float8)
STABLE PARALLEL RESTRICTED LANGUAGE c AS 'MODULE_PATHNAME', 'minimum_spanning_forest_wrapper';
"#
)]
fn minimum_spanning_forest(
    graph: Option<&str>,
    _id_type: Option<AnyElement>,
    rels: Option<Vec<Option<String>>>,
    weighted: Option<bool>,
    version: Option<i64>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> TableIterator<
    'static,
    (name!(source, AnyElement), name!(target, AnyElement), name!(weight, f64)),
> {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let resolved = resolve(graph, version, "read")?;
        let rows = with_id_type!(resolved.graph.id_type, E => {
            // SAFETY: fcinfo is this call's, and argument 1 exists.
            let witness = unsafe { pgrx::fcinfo::pg_getarg_type(fcinfo, 1) };
            check_type::<E>(graph, "id_type", witness, false)?;
            forest_rows::<E>(&resolved, rels.as_deref(), weighted)?
        });
        Ok(TableIterator::new(rows))
    })
}

/// The forest's edges as rows of external ids.
fn forest_rows<E: CachedIds>(
    resolved: &Resolved,
    rels: Option<&[Option<String>]>,
    weighted: Option<bool>,
) -> Result<ForestRows, ComputeSqlError> {
    let loaded = cached::<E>(resolved)?;
    let memory = compute_memory();
    let selected = relations(&loaded.relations, rels);
    let forest = compute::minimum_spanning_forest(
        &*workers()?,
        &memory,
        &mut PostgresInterrupts,
        &loaded.graph,
        selection(selected.as_deref()),
        weighted,
    )?;
    // Each row's external ids are made when it is returned (DESIGN.md
    // §12): the forest stays in its charged buffer, and only the current
    // row's ids are palloc'd, in the context Postgres resets between rows.
    // The rows hold the loaded version, which keeps it mapped until the
    // call ends, however it ends.
    Ok(Box::new((0..forest.edges.len()).filter_map(move |edge| {
        let &(source, target, weight) = forest.edges.get(edge)?;
        Some((external(&loaded.id_map, source)?, external(&loaded.id_map, target)?, weight))
    })))
}

/// The rows of a minimum spanning forest, made one at a time.
type ForestRows = Box<dyn Iterator<Item = (AnyElement, AnyElement, f64)>>;

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, held_after_first_row, iri_path, refresh, rows, small};

    /// The live rows of vector `name` of graph `graph`, as `id:value`.
    fn values(graph: &str, name: &str) -> Result<Option<String>, Error> {
        rows(&format!(
            "SELECT node_id || ':' || value FROM warren.vector_values('{graph}', '{name}', NULL::bigint)"
        ))
    }

    /// Graph `k` over `k_nodes`/`k_edges`: a 4-clique on 1–4 (relation
    /// `a`, some edges both ways), a tail 4 → 5 → 6 in `b`, a self-loop on
    /// 6 and node 7 alone; no reverse CSR.
    fn clique_with_a_tail() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE k_nodes (id bigint);
             CREATE TABLE k_edges (src bigint, dst bigint, rel text);
             INSERT INTO k_nodes SELECT generate_series(1, 7);
             INSERT INTO k_edges SELECT a, b, 'a' FROM generate_series(1, 4) a, generate_series(1, 4) b
                 WHERE a < b OR a = 3;
             INSERT INTO k_edges VALUES (4, 5, 'b'), (5, 6, 'b'), (6, 6, 'b');
             SELECT warren.create_graph('k', 'SELECT id FROM k_nodes',
                                        'SELECT src, dst, rel FROM k_edges', has_relations := true,
                                        build_reverse := false);
             SELECT warren.refresh('k')",
        )
    }

    /// Each forest edge's text ids are made when its row is returned, so a
    /// call holds no more Postgres memory for 49,999 rows to come than for
    /// 999 (built up front, the ids took about 72 bytes each).
    #[pg_test]
    fn the_forest_makes_each_rows_ids_as_it_is_returned() -> Result<(), Error> {
        iri_path("few", 1_000)?;
        iri_path("many", 50_000)?;
        let few = held_after_first_row("warren.minimum_spanning_forest('few', NULL::text)")?;
        let many = held_after_first_row("warren.minimum_spanning_forest('many', NULL::text)")?;
        assert!(many < few + (256 << 10), "{many} bytes held for 49,999 rows, {few} for 999");
        Ok(())
    }

    #[pg_test]
    fn core_numbers_ignore_direction_and_self_loops_and_return_the_degeneracy() -> Result<(), Error>
    {
        clique_with_a_tail()?;
        let degeneracy = Spi::get_one::<i64>("SELECT warren.core_number('k', 'core')")?;
        assert_eq!(degeneracy, Some(3));
        assert_eq!(values("k", "core")?.as_deref(), Some("1:3 2:3 3:3 4:3 5:1 6:1 7:0"));
        let tail = Spi::get_one::<i64>("SELECT warren.core_number('k', 'b', ARRAY['b'])")?;
        assert_eq!(tail, Some(1));
        assert_eq!(values("k", "b")?.as_deref(), Some("1:0 2:0 3:0 4:1 5:1 6:1 7:0"));
        let dtype = rows("SELECT dtype FROM warren.vectors WHERE name = 'core'")?;
        assert_eq!(dtype.as_deref(), Some("i64"));
        Ok(())
    }

    #[pg_test]
    fn core_number_writes_transient_vectors_and_refuses_null_arguments() -> Result<(), Error> {
        small("t")?;
        let degeneracy =
            Spi::get_one::<i64>("SELECT warren.core_number('t', 'c', persist := false)")?;
        assert_eq!(degeneracy, Some(2));
        assert_eq!(values("t", "c")?.as_deref(), Some("1:2 2:2 3:2 4:2 5:0 6:0"));
        let persisted = rows("SELECT name FROM warren.vectors")?;
        assert_eq!(persisted.as_deref(), Some(""));
        for (call, argument) in [
            ("warren.core_number(NULL, 'c')", "graph"),
            ("warren.core_number('t', NULL)", "out"),
            ("warren.core_number('t', 'c', persist := NULL)", "persist"),
        ] {
            let refused = failure(call)?.unwrap_or_default();
            assert_eq!(refused, format!("22004: {argument} must not be NULL"), "{call}");
        }
        Ok(())
    }

    #[pg_test]
    fn strong_components_are_labelled_by_their_smallest_dense_id() -> Result<(), Error> {
        small("t")?;
        let count = Spi::get_one::<i64>("SELECT warren.strong_components_vector('t', 'scc')")?;
        assert_eq!(count, Some(3));
        assert_eq!(values("t", "scc")?.as_deref(), Some("1:0 2:0 3:0 4:0 5:4 6:5"));
        let count = Spi::get_one::<i64>(
            "SELECT warren.strong_components_vector('t', 'a', ARRAY['a'], persist := false)",
        )?;
        assert_eq!(count, Some(6));
        assert_eq!(values("t", "a")?.as_deref(), Some("1:0 2:1 3:2 4:3 5:4 6:5"));
        Spi::run("DELETE FROM t_nodes WHERE id = 2")?;
        refresh("t", true)?;
        let count = Spi::get_one::<i64>("SELECT warren.strong_components_vector('t', 'scc')")?;
        assert_eq!(count, Some(4), "the tombstoned node is not counted");
        assert_eq!(values("t", "scc")?.as_deref(), Some("1:0 3:2 4:0 5:4 6:5"));
        Ok(())
    }

    /// Graph `d` over `d_nodes`/`d_edges`: the weighted DAG 1 → 2 (2),
    /// 2 → 4 (−5), 1 → 3 (1), 3 → 4 (1), 4 → 5 (4), and node 6 alone.
    fn weighted_dag() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE d_nodes (id bigint);
             CREATE TABLE d_edges (src bigint, dst bigint, w float8);
             INSERT INTO d_nodes SELECT generate_series(1, 6);
             INSERT INTO d_edges VALUES (1, 2, 2), (2, 4, -5), (1, 3, 1), (3, 4, 1), (4, 5, 4);
             SELECT warren.create_graph('d', 'SELECT id FROM d_nodes',
                                        'SELECT src, dst, w FROM d_edges', weighted := true,
                                        build_reverse := false);
             SELECT warren.refresh('d')",
        )
    }

    #[pg_test]
    fn topological_order_takes_the_smallest_ready_node_first() -> Result<(), Error> {
        weighted_dag()?;
        Spi::run("SELECT warren.topological_order('d', 'o')")?;
        assert_eq!(values("d", "o")?.as_deref(), Some("1:0 2:1 3:2 4:3 5:4 6:5"));
        small("t")?;
        Spi::run("SELECT warren.topological_order('t', 'a', ARRAY['a'])")?;
        assert_eq!(values("t", "a")?.as_deref(), Some("1:1 2:2 3:3 4:0 5:4 6:5"));
        let dtype = rows("SELECT dtype FROM warren.vectors WHERE name = 'a'")?;
        assert_eq!(dtype.as_deref(), Some("i64"));
        Ok(())
    }

    #[pg_test]
    fn a_cycle_has_no_topological_order_or_longest_path() -> Result<(), Error> {
        small("t")?;
        let cycle = "22023: the graph has a cycle: 5 nodes are on a cycle or after one, so it has \
                     no topological order";
        let refused = failure("warren.topological_order('t', 'o')")?;
        assert_eq!(refused.as_deref(), Some(cycle));
        let refused = failure("warren.dag_longest_path('t', 'l')")?;
        assert_eq!(refused.as_deref(), Some(cycle));
        let written = rows("SELECT name FROM warren.vectors")?;
        assert_eq!(written.as_deref(), Some(""), "nothing is written");
        Ok(())
    }

    #[pg_test]
    fn longest_paths_end_at_each_node_and_the_longest_is_returned() -> Result<(), Error> {
        weighted_dag()?;
        let longest = Spi::get_one::<f64>("SELECT warren.dag_longest_path('d', 'l')")?;
        assert_eq!(longest, Some(6.0));
        assert_eq!(values("d", "l")?.as_deref(), Some("1:0 2:2 3:1 4:2 5:6 6:0"));
        let hops = Spi::get_one::<f64>(
            "SELECT warren.dag_longest_path('d', 'h', weighted := false, persist := false)",
        )?;
        assert_eq!(hops, Some(3.0));
        assert_eq!(values("d", "h")?.as_deref(), Some("1:0 2:1 3:1 4:2 5:3 6:0"));
        let dtype = rows("SELECT dtype FROM warren.vectors WHERE name = 'l'")?;
        assert_eq!(dtype.as_deref(), Some("f64"));
        Ok(())
    }

    #[pg_test]
    fn the_spanning_forest_keeps_the_lighter_direction_of_each_pair() -> Result<(), Error> {
        small("t")?;
        let forest = |rels: &str| {
            rows(&format!(
                "SELECT source || '>' || target || ':' || weight
                 FROM warren.minimum_spanning_forest('t', NULL::bigint, {rels})"
            ))
        };
        assert_eq!(forest("NULL")?.as_deref(), Some("1>2:1 2>3:1 4>1:1"));
        assert_eq!(forest("ARRAY['b']")?.as_deref(), Some("1>4:5 3>4:1"));
        let unweighted = rows(
            "SELECT count(*) || ':' || sum(weight)
             FROM warren.minimum_spanning_forest('t', NULL::bigint, weighted := false)",
        )?;
        assert_eq!(unweighted.as_deref(), Some("3:3"));
        let refused = failure("warren.minimum_spanning_forest('t', NULL::uuid)")?;
        assert_eq!(
            refused.as_deref(),
            Some(
                "42804: graph \"t\" has node ids of type bigint, but id_type is of type uuid; \
                 cast it to bigint"
            )
        );
        Ok(())
    }

    #[pg_test]
    fn the_structural_kernels_refuse_null_arguments() -> Result<(), Error> {
        small("t")?;
        for (call, argument) in [
            ("warren.strong_components_vector(NULL, 'c')", "graph"),
            ("warren.strong_components_vector('t', NULL)", "out"),
            ("warren.topological_order('t', NULL)", "out"),
            ("warren.topological_order('t', 'o', persist := NULL)", "persist"),
            ("warren.dag_longest_path(NULL, 'l')", "graph"),
            ("warren.dag_longest_path('t', NULL)", "out"),
            ("warren.minimum_spanning_forest(NULL, NULL::bigint)", "graph"),
        ] {
            let refused = failure(call)?.unwrap_or_default();
            assert_eq!(refused, format!("22004: {argument} must not be NULL"), "{call}");
        }
        Ok(())
    }
}
