//! `warren.local_moving` and `warren.leiden_refine` (DESIGN.md §6.7): the
//! two phases of a Louvain or Leiden level as primitives over a version's
//! undirected weighted view. `warren.louvain` and `warren.leiden` are
//! PL/pgSQL over them, `contract`, `project` and `lift`.

use pgrx::prelude::*;
use warren_core::community::{local_moving as moving, refine, LocalMoving, Refinement};

use super::modularity::require_resolution;
use super::partition::{integers, undirected};
use super::CommunitySqlError;
use crate::access::entry;
use crate::activity::Running;
use crate::cache::resolve;
use crate::compute_sql::Vectors;
use crate::interrupts::workers;
use crate::interrupts::PostgresInterrupts;
use crate::source::with_id_type;

/// `warren.local_moving`: Louvain's local moving on `graph`'s version
/// `version` (NULL: the current one), writing vector `out` (`i64`): each
/// live node's community, labelled by the smallest of its members' `keys`
/// (a vector of integers; NULL: dense ids). Nodes start in the communities
/// vector `initial` gives them (NULL: each alone) and move, sweep after
/// sweep, to the neighbouring community that most raises modularity at
/// `resolution`, until a sweep moves none, raises modularity by no more
/// than `tolerance`, or `max_sweeps` have run. The graph is undirected for
/// this (`warren.modularity`'s view, `weighted` and `per_relation` as
/// there); `seed` orders the sweep. Returns the number of communities.
#[allow(clippy::too_many_arguments)] // the SQL signature of DESIGN.md §6.7
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn local_moving(
    graph: Option<&str>,
    out: Option<&str>,
    initial: default!(Option<&str>, "NULL"),
    keys: default!(Option<&str>, "NULL"),
    resolution: default!(Option<f64>, "1.0"),
    seed: default!(Option<i64>, "0"),
    max_sweeps: default!(Option<i32>, "100"),
    tolerance: default!(Option<f64>, "1e-7"),
    weighted: default!(Option<bool>, "NULL"),
    persist: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
    per_relation: default!(Option<bool>, "false"),
) -> Option<i64> {
    entry(|_| -> Result<Option<i64>, CommunitySqlError> {
        let graph = graph.ok_or(CommunitySqlError::NullArgument("graph"))?;
        let out = out.ok_or(CommunitySqlError::NullArgument("out"))?;
        let resolution = resolution.ok_or(CommunitySqlError::NullArgument("resolution"))?;
        let seed = seed.ok_or(CommunitySqlError::NullArgument("seed"))?;
        let max_sweeps = max_sweeps.ok_or(CommunitySqlError::NullArgument("max_sweeps"))?;
        let tolerance = tolerance.ok_or(CommunitySqlError::NullArgument("tolerance"))?;
        let persist = persist.ok_or(CommunitySqlError::NullArgument("persist"))?;
        require_resolution(resolution)?;
        let max_sweeps =
            u32::try_from(max_sweeps).ok().filter(|&sweeps| sweeps >= 1).ok_or_else(|| {
                CommunitySqlError::InvalidArgument(format!(
                    "max_sweeps must be at least 1, not {max_sweeps}"
                ))
            })?;
        if tolerance.is_nan() || tolerance < 0.0 {
            return Err(CommunitySqlError::InvalidArgument(format!(
                "tolerance must be a number of at least 0, not {tolerance}"
            )));
        }
        let options = LocalMoving { resolution, seed: seed as u64, max_sweeps, tolerance };
        let resolved = resolve(graph, version, "write vectors of")?;
        let _running =
            Running::start("local_moving", Some(resolved.graph.graph_id), Some(resolved.version));
        with_id_type!(resolved.graph.id_type, E => {
            let mut vectors = Vectors::<E>::open(resolved)?;
            vectors.check_writable(out, persist)?;
            let initial = initial.map(|name| integers(&mut vectors, name, "initial")).transpose()?;
            let keys = keys.map(|name| integers(&mut vectors, name, "keys")).transpose()?;
            let view = undirected(&vectors, weighted, per_relation)?;
            let moved = moving(
                &*workers()?,
                &vectors.memory,
                &mut PostgresInterrupts,
                &view,
                vectors.tombstones(),
                initial.as_ref().map(|(labels, _)| labels.values()),
                keys.as_ref().map(|(keys, _)| keys.values()),
                options,
            )?;
            drop((view, initial, keys));
            vectors.save(out, moved.labels.into(), persist)?;
            Ok(Some(moved.communities as i64))
        })
    })
}

/// `warren.leiden_refine`: Leiden's refinement of the partition vector
/// `communities` gives `graph`'s version `version` (NULL: the current
/// one), writing vector `out` (`i64`): each live node's refined community,
/// inside its community and connected, labelled by the smallest of its
/// members' `keys` (NULL: dense ids). `randomness` is θ of Traag et al.
/// (0: always the best merge); `seed` orders the nodes and seeds the
/// draws; `weighted` and `per_relation` are `warren.modularity`'s. Returns
/// the number of refined communities.
#[allow(clippy::too_many_arguments)] // the SQL signature of DESIGN.md §6.7
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn leiden_refine(
    graph: Option<&str>,
    communities: Option<&str>,
    out: Option<&str>,
    keys: default!(Option<&str>, "NULL"),
    resolution: default!(Option<f64>, "1.0"),
    seed: default!(Option<i64>, "0"),
    randomness: default!(Option<f64>, "0.01"),
    weighted: default!(Option<bool>, "NULL"),
    persist: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
    per_relation: default!(Option<bool>, "false"),
) -> Option<i64> {
    entry(|_| -> Result<Option<i64>, CommunitySqlError> {
        let graph = graph.ok_or(CommunitySqlError::NullArgument("graph"))?;
        let communities = communities.ok_or(CommunitySqlError::NullArgument("communities"))?;
        let out = out.ok_or(CommunitySqlError::NullArgument("out"))?;
        let resolution = resolution.ok_or(CommunitySqlError::NullArgument("resolution"))?;
        let seed = seed.ok_or(CommunitySqlError::NullArgument("seed"))?;
        let randomness = randomness.ok_or(CommunitySqlError::NullArgument("randomness"))?;
        let persist = persist.ok_or(CommunitySqlError::NullArgument("persist"))?;
        require_resolution(resolution)?;
        if !(randomness.is_finite() && randomness >= 0.0) {
            return Err(CommunitySqlError::InvalidArgument(format!(
                "randomness must be a finite number of at least 0, not {randomness}"
            )));
        }
        let options = Refinement { resolution, seed: seed as u64, randomness };
        let resolved = resolve(graph, version, "write vectors of")?;
        let _running =
            Running::start("leiden_refine", Some(resolved.graph.graph_id), Some(resolved.version));
        with_id_type!(resolved.graph.id_type, E => {
            let mut vectors = Vectors::<E>::open(resolved)?;
            vectors.check_writable(out, persist)?;
            let (partition, _charge) = integers(&mut vectors, communities, "communities")?;
            let keys = keys.map(|name| integers(&mut vectors, name, "keys")).transpose()?;
            let view = undirected(&vectors, weighted, per_relation)?;
            let refined = refine(
                &*workers()?,
                &vectors.memory,
                &mut PostgresInterrupts,
                &view,
                vectors.tombstones(),
                partition.values(),
                keys.as_ref().map(|(keys, _)| keys.values()),
                options,
            )?;
            drop((view, keys));
            vectors.save(out, refined.labels.into(), persist)?;
            Ok(Some(refined.communities as i64))
        })
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::super::modularity::tests::two_triangles;
    use crate::fixture::{failure, rows};

    fn values(name: &str) -> Result<Option<String>, Error> {
        rows(&format!(
            "SELECT node_id || ':' || value FROM warren.vector_values('tt', '{name}', NULL::bigint)"
        ))
    }

    #[pg_test]
    fn local_moving_finds_the_two_triangles() -> Result<(), Error> {
        two_triangles()?;
        let found = Spi::get_one::<i64>("SELECT warren.local_moving('tt', 'c')")?;
        assert_eq!(found, Some(2));
        assert_eq!(values("c")?.as_deref(), Some("1:0 2:0 3:0 4:3 5:3 6:3"));
        Spi::run("SELECT warren.vector_from_query('tt', 'k', 'SELECT id, 10 * id FROM tt_nodes')")?;
        Spi::run("SELECT warren.local_moving('tt', 'ck', keys := 'k', persist := false)")?;
        assert_eq!(values("ck")?.as_deref(), Some("1:10 2:10 3:10 4:40 5:40 6:40"));
        Ok(())
    }

    #[pg_test]
    fn local_moving_starts_from_an_initial_partition() -> Result<(), Error> {
        two_triangles()?;
        let found = Spi::get_one::<i64>(
            "SELECT warren.local_moving('tt', 'c', initial := 'halves', max_sweeps := 1)",
        )?;
        assert_eq!(found, Some(2));
        Ok(())
    }

    #[pg_test]
    fn leiden_refine_keeps_refined_communities_inside_their_community() -> Result<(), Error> {
        two_triangles()?;
        Spi::run("SELECT warren.vector_const('tt', 'one', 1, 'i64')")?;
        let found =
            Spi::get_one::<i64>("SELECT warren.leiden_refine('tt', 'one', 'r', randomness := 0)")?;
        assert!(found.is_some_and(|n| (1..=6).contains(&n)), "{found:?}");
        let refined = Spi::get_one::<i64>("SELECT warren.leiden_refine('tt', 'halves', 'r2')")?;
        let crossing = Spi::get_one::<i64>(
            "SELECT count(*) FROM warren.vector_values('tt', 'r2', NULL::bigint) a
             JOIN warren.vector_values('tt', 'r2', NULL::bigint) b USING (value)
             WHERE (a.node_id - 1) / 3 <> (b.node_id - 1) / 3",
        )?;
        assert!(refined.is_some());
        assert_eq!(crossing, Some(0));
        Ok(())
    }

    #[pg_test]
    fn the_primitives_refuse_bad_arguments() -> Result<(), Error> {
        two_triangles()?;
        assert_eq!(
            failure("warren.local_moving('tt', 'c', max_sweeps := 0)")?.as_deref(),
            Some("22023: max_sweeps must be at least 1, not 0")
        );
        for tolerance in ["-1", "'NaN'"] {
            assert_eq!(
                failure(&format!("warren.local_moving('tt', 'c', tolerance := {tolerance})"))?,
                Some(format!(
                    "22023: tolerance must be a number of at least 0, not {}",
                    if tolerance == "-1" { "-1" } else { "NaN" }
                ))
            );
        }
        assert_eq!(
            failure("warren.leiden_refine('tt', 'halves', 'r', randomness := -1)")?.as_deref(),
            Some("22023: randomness must be a finite number of at least 0, not -1")
        );
        assert_eq!(
            failure("warren.local_moving('tt', '_warren_x')")?.as_deref(),
            Some(
                "42939: vector names beginning with _warren_ are reserved for the library's \
                 intermediates, and are only ever transient: \"_warren_x\""
            )
        );
        Spi::run(
            "CREATE TABLE neg_nodes (id bigint); CREATE TABLE neg_edges (src bigint, dst bigint, w float8);
             INSERT INTO neg_nodes VALUES (1), (2); INSERT INTO neg_edges VALUES (1, 2, -1);
             SELECT warren.create_graph('neg', 'SELECT id FROM neg_nodes',
                                        'SELECT src, dst, w FROM neg_edges', weighted := true);
             SELECT warren.refresh('neg')",
        )?;
        assert_eq!(
            failure("warren.local_moving('neg', 'c')")?.as_deref(),
            Some("22023: the edge 1 → 2 weighs -1: modularity needs weights of at least 0")
        );
        Ok(())
    }
}
