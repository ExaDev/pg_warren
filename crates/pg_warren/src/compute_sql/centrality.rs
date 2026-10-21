//! Shortest-path centralities of the algorithm library (DESIGN.md §6.7,
//! planned library group 2): `betweenness_centrality`,
//! `closeness_centrality` and `harmonic_centrality`. Each runs a
//! `warren_core::compute` kernel on the compute pool within the call's
//! memory budget, from every live node or from `k` sources sampled with
//! `seed` (among the nodes vector `sources` selects, if given), writes a
//! named `f64` vector and returns the number of sources.

use warren_core::compute::{
    self, Centrality, ComputeError, ComputeMemory, Mask, Sampling, ShortestPaths, Workers,
};

use pgrx::prelude::*;

use super::settings::workers;
use super::vectors::Vectors;
use super::{entry, ComputeSqlError};
use crate::cache::resolve;
use crate::interrupts::PostgresInterrupts;
use crate::selection::relations;
use crate::source::with_id_type;

/// How a centrality's sources are chosen and its paths followed, as the
/// SQL arguments give them.
struct Arguments<'a> {
    k: Option<i64>,
    seed: Option<i64>,
    sources: Option<&'a str>,
    directed: Option<bool>,
    rels: Option<Vec<Option<String>>>,
    weighted: Option<bool>,
}

/// Runs `kernel` on graph `graph` as `arguments` ask and saves its vector
/// as `out`; returns the number of sources.
fn centrality(
    graph: Option<&str>,
    out: Option<&str>,
    persist: Option<bool>,
    version: Option<i64>,
    arguments: Arguments<'_>,
    kernel: impl FnOnce(
        &Workers,
        &ComputeMemory,
        &ShortestPaths<'_, '_>,
        &Sampling<'_>,
    ) -> Result<Centrality, ComputeError>,
) -> Result<Option<i64>, ComputeSqlError> {
    let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
    let out = out.ok_or(ComputeSqlError::NullArgument("out"))?;
    let persist = persist.ok_or(ComputeSqlError::NullArgument("persist"))?;
    let seed = arguments.seed.ok_or(ComputeSqlError::NullArgument("seed"))?;
    let directed = arguments.directed.ok_or(ComputeSqlError::NullArgument("directed"))?;
    let k = match arguments.k {
        Some(k) if k < 1 => {
            return Err(ComputeSqlError::InvalidArgument(format!("k must be at least 1, not {k}")))
        }
        k => k.map(|k| k as u64),
    };
    let resolved = resolve(graph, version, "write vectors of")?;
    with_id_type!(resolved.graph.id_type, E => {
        let mut vectors = Vectors::<E>::open(resolved)?;
        let population = arguments.sources.map(|name| vectors.get(name)).transpose()?;
        let loaded = &vectors.loaded;
        let selected = relations(&loaded.relations, arguments.rels.as_deref());
        let paths = ShortestPaths {
            graph: &loaded.graph,
            relations: super::structure::selection(selected.as_deref()),
            tombstones: loaded.id_map.tombstones(),
            directed,
            weighted: arguments.weighted,
        };
        let population = population.as_deref().map(|vector| Mask { vector, complement: false });
        let sampling = Sampling { k, seed: seed as u64, population };
        let found = kernel(&*workers()?, &vectors.memory, &paths, &sampling)?;
        vectors.save(out, found.values.into(), persist)?;
        Ok(Some(found.sources as i64))
    })
}

/// `warren.betweenness_centrality` (§6.7): vector `out` (`f64`) holding
/// each node's betweenness over the shortest paths of the relations `rels`
/// (all if NULL), along edges if `directed`, else in the undirected simple
/// graph, weighted if `weighted` (NULL: if the graph has weights), as
/// NetworkX's `betweenness_centrality(G, k, normalized, weight,
/// endpoints)`: from every live node if `k` is NULL, else from `k` sampled
/// with `seed` (among the nodes vector `sources` selects, if given).
/// Returns the number of sources.
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
#[allow(clippy::too_many_arguments)] // The SQL function's arguments.
fn betweenness_centrality(
    graph: Option<&str>,
    out: Option<&str>,
    k: default!(Option<i64>, "NULL"),
    seed: default!(Option<i64>, "0"),
    normalized: default!(Option<bool>, "true"),
    endpoints: default!(Option<bool>, "false"),
    directed: default!(Option<bool>, "true"),
    sources: default!(Option<&str>, "NULL"),
    rels: default!(Option<Vec<Option<String>>>, "NULL"),
    weighted: default!(Option<bool>, "NULL"),
    persist: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
) -> Option<i64> {
    entry(|_| {
        let normalized = normalized.ok_or(ComputeSqlError::NullArgument("normalized"))?;
        let endpoints = endpoints.ok_or(ComputeSqlError::NullArgument("endpoints"))?;
        let scale = compute::BetweennessScale { normalized, endpoints };
        let arguments = Arguments { k, seed, sources, directed, rels, weighted };
        centrality(graph, out, persist, version, arguments, |workers, memory, paths, sampling| {
            let interrupts = &mut PostgresInterrupts;
            compute::betweenness_centrality(workers, memory, interrupts, paths, sampling, scale)
        })
    })
}

/// `warren.closeness_centrality` (§6.7): vector `out` (`f64`) holding
/// each node's closeness over the distances to it (along edges if
/// `directed`, NetworkX's incoming distance; else in the undirected simple
/// graph), scaled by the fraction of nodes that reach it if `wf_improved`,
/// as NetworkX's `closeness_centrality(G, distance, wf_improved)`: exact if
/// `k` is NULL, else estimated from `k` sources sampled with `seed` (among
/// the nodes vector `sources` selects, if given). `rels`, `weighted` as
/// `betweenness_centrality`'s. Returns the number of sources.
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
#[allow(clippy::too_many_arguments)] // The SQL function's arguments.
fn closeness_centrality(
    graph: Option<&str>,
    out: Option<&str>,
    k: default!(Option<i64>, "NULL"),
    seed: default!(Option<i64>, "0"),
    wf_improved: default!(Option<bool>, "true"),
    directed: default!(Option<bool>, "true"),
    sources: default!(Option<&str>, "NULL"),
    rels: default!(Option<Vec<Option<String>>>, "NULL"),
    weighted: default!(Option<bool>, "NULL"),
    persist: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
) -> Option<i64> {
    entry(|_| {
        let wf_improved = wf_improved.ok_or(ComputeSqlError::NullArgument("wf_improved"))?;
        let arguments = Arguments { k, seed, sources, directed, rels, weighted };
        centrality(graph, out, persist, version, arguments, |workers, memory, paths, sampling| {
            let interrupts = &mut PostgresInterrupts;
            compute::closeness_centrality(workers, memory, interrupts, paths, sampling, wf_improved)
        })
    })
}

/// `warren.harmonic_centrality` (§6.7): vector `out` (`f64`) holding the
/// sum of each node's reciprocal distances from the other nodes, as
/// NetworkX's `harmonic_centrality(G, distance)`: exact if `k` is NULL,
/// else estimated from `k` sources sampled with `seed` (among the nodes
/// vector `sources` selects, if given), scaled by `(n − 1) / k_u`. The
/// other arguments as `closeness_centrality`'s. Returns the number of
/// sources.
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
#[allow(clippy::too_many_arguments)] // The SQL function's arguments.
fn harmonic_centrality(
    graph: Option<&str>,
    out: Option<&str>,
    k: default!(Option<i64>, "NULL"),
    seed: default!(Option<i64>, "0"),
    directed: default!(Option<bool>, "true"),
    sources: default!(Option<&str>, "NULL"),
    rels: default!(Option<Vec<Option<String>>>, "NULL"),
    weighted: default!(Option<bool>, "NULL"),
    persist: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
) -> Option<i64> {
    entry(|_| {
        let arguments = Arguments { k, seed, sources, directed, rels, weighted };
        centrality(graph, out, persist, version, arguments, |workers, memory, paths, sampling| {
            let interrupts = &mut PostgresInterrupts;
            compute::harmonic_centrality(workers, memory, interrupts, paths, sampling)
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

    /// The live rows of vector `name` of graph `t`, as `id:value`.
    fn values(name: &str) -> Result<Option<String>, Error> {
        rows(&format!(
            "SELECT node_id || ':' || value FROM warren.vector_values('t', '{name}', NULL::bigint)"
        ))
    }

    #[pg_test]
    fn betweenness_is_networkx_s_weighted_unweighted_and_undirected() -> Result<(), Error> {
        small("t")?;
        let sources = Spi::get_one::<i64>(
            "SELECT warren.betweenness_centrality('t', 'b', normalized := false)",
        )?;
        assert_eq!(sources, Some(6));
        assert_eq!(values("b")?.as_deref(), Some("1:3 2:3 3:3 4:3 5:0 6:0"));
        Spi::run("SELECT warren.betweenness_centrality('t', 'bu', normalized := false, weighted := false)")?;
        assert_eq!(values("bu")?.as_deref(), Some("1:3 2:2 3:2 4:3 5:0 6:0"));
        Spi::run("SELECT warren.betweenness_centrality('t', 'bn')")?;
        let normalized = "1:0.15000000000000002 2:0.15000000000000002 3:0.15000000000000002 \
                          4:0.15000000000000002 5:0 6:0";
        assert_eq!(values("bn")?.as_deref(), Some(normalized));
        Spi::run(
            "SELECT warren.betweenness_centrality('t', 'bd', normalized := false, directed := false,
                                                  weighted := false, persist := false)",
        )?;
        assert_eq!(values("bd")?.as_deref(), Some("1:0.5 2:0.5 3:0.5 4:0.5 5:0 6:0"));
        let dtype = rows("SELECT dtype FROM warren.vectors WHERE name = 'b'")?;
        assert_eq!(dtype.as_deref(), Some("f64"));
        Ok(())
    }

    #[pg_test]
    fn sources_are_sampled_with_a_seed_or_taken_from_a_vector() -> Result<(), Error> {
        small("t")?;
        let sampled = "SELECT warren.harmonic_centrality('t', 'h', k := 2, seed := 7)";
        assert_eq!(Spi::get_one::<i64>(sampled)?, Some(2));
        let first = values("h")?;
        Spi::run(sampled)?;
        assert_eq!(values("h")?, first, "a seed samples the same sources");
        assert_eq!(
            Spi::get_one::<i64>("SELECT warren.harmonic_centrality('t', 'h', k := 99)")?,
            Some(6)
        );
        Spi::run(
            "SELECT warren.vector_from_query('t', 'from', 'SELECT id, true FROM t_nodes WHERE id IN (1, 3)',
                                             dtype := 'bool', persist := false)",
        )?;
        let from =
            "SELECT warren.closeness_centrality('t', 'c', sources := 'from', weighted := false)";
        assert_eq!(Spi::get_one::<i64>(from)?, Some(2));
        // From 1: 2 at 1, 3 at 2, 4 at 1; from 3: 4 at 1, 1 at 2, 2 at 3.
        // Node 4: both reach it, total 2, k_u = 2: 2/2 · 2/2.
        assert_eq!(values("c")?.as_deref(), Some("1:0.5 2:0.5 3:0.5 4:1 5:0 6:0"));
        Ok(())
    }

    #[pg_test]
    fn closeness_and_harmonic_use_weighted_incoming_distances() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "SELECT warren.closeness_centrality('t', 'c'), warren.harmonic_centrality('t', 'h')",
        )?;
        assert_eq!(values("c")?.as_deref(), Some("1:0.3 2:0.3 3:0.3 4:0.3 5:0 6:0"));
        let harmonic = "1:1.8333333333333333 2:1.8333333333333333 3:1.8333333333333333 \
                        4:1.8333333333333333 5:0 6:0";
        assert_eq!(values("h")?.as_deref(), Some(harmonic));
        Ok(())
    }

    #[pg_test]
    fn the_centralities_refuse_bad_arguments_and_weights() -> Result<(), Error> {
        small("t")?;
        for (call, refused) in [
            ("warren.betweenness_centrality(NULL, 'b')", "22004: graph must not be NULL"),
            ("warren.betweenness_centrality('t', NULL)", "22004: out must not be NULL"),
            (
                "warren.betweenness_centrality('t', 'b', normalized := NULL)",
                "22004: normalized must not be NULL",
            ),
            (
                "warren.betweenness_centrality('t', 'b', endpoints := NULL)",
                "22004: endpoints must not be NULL",
            ),
            (
                "warren.betweenness_centrality('t', 'b', seed := NULL)",
                "22004: seed must not be NULL",
            ),
            (
                "warren.closeness_centrality('t', 'c', wf_improved := NULL)",
                "22004: wf_improved must not be NULL",
            ),
            (
                "warren.closeness_centrality('t', 'c', directed := NULL)",
                "22004: directed must not be NULL",
            ),
            (
                "warren.harmonic_centrality('t', 'h', persist := NULL)",
                "22004: persist must not be NULL",
            ),
            ("warren.harmonic_centrality('t', 'h', k := 0)", "22023: k must be at least 1, not 0"),
            (
                "warren.harmonic_centrality('t', 'h', sources := 'none')",
                "42704: graph \"t\" has no vector \"none\" at version",
            ),
        ] {
            let found = failure(call)?.unwrap_or_default();
            assert!(found.starts_with(refused), "{call}: {found}");
        }
        Spi::run("INSERT INTO t_edges VALUES (2, 5, 'a', 0)")?;
        refresh("t", true)?;
        let refused = failure("warren.betweenness_centrality('t', 'b')")?;
        let expected = "22023: weighted shortest paths need positive, finite edge weights";
        assert_eq!(refused.as_deref(), Some(expected));
        assert!(failure("warren.betweenness_centrality('t', 'b', weighted := false)")?.is_none());
        Ok(())
    }
}
