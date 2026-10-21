//! `warren.modularity` (DESIGN.md §6.7): the modularity of a partition of
//! a version, given as a label vector, on the version's undirected
//! weighted view.

use pgrx::prelude::*;
use warren_core::community::modularity as score;

use super::partition::{integers, undirected};
use super::CommunitySqlError;
use crate::access::entry;
use crate::cache::resolve;
use crate::compute_sql::Vectors;
use crate::source::with_id_type;

/// `warren.modularity`: the modularity, at resolution `resolution`, of the
/// partition that vector `labels` (integers, one per node) gives `graph`'s
/// version `version` (NULL: the current one): directions ignored (an
/// edge and its reverse add up), edges weighted if `weighted` (NULL: if
/// the graph has weights), an undirected self-loop counted once in its
/// node's degree, as `contract` counts it, so a partition scores as the
/// singletons of its quotient; 0 for a graph without edges. On an
/// undirected graph it is NetworkX's `community.modularity` with each
/// self-loop's weight halved (equal without self-loops); on a directed
/// graph, NetworkX's of `nx.MultiGraph(G)`, not of `G`, which NetworkX
/// scores with Leicht and Newman's directed formula.
#[pg_extern(schema = "warren", stable, parallel_restricted)]
fn modularity(
    graph: Option<&str>,
    labels: Option<&str>,
    resolution: default!(Option<f64>, "1.0"),
    weighted: default!(Option<bool>, "NULL"),
    version: default!(Option<i64>, "NULL"),
) -> Option<f64> {
    entry(|_| -> Result<Option<f64>, CommunitySqlError> {
        let graph = graph.ok_or(CommunitySqlError::NullArgument("graph"))?;
        let labels = labels.ok_or(CommunitySqlError::NullArgument("labels"))?;
        let resolution = resolution.ok_or(CommunitySqlError::NullArgument("resolution"))?;
        require_resolution(resolution)?;
        let resolved = resolve(graph, version, "read")?;
        with_id_type!(resolved.graph.id_type, E => {
            let mut vectors = Vectors::<E>::open(resolved)?;
            let (labels, _charge) = integers(&mut vectors, labels, "labels")?;
            let view = undirected(&vectors, weighted)?;
            Ok(Some(score(&vectors.memory, &view, labels.values(), resolution)?))
        })
    })
}

/// Refuses a resolution that is negative or not finite.
pub(super) fn require_resolution(resolution: f64) -> Result<(), CommunitySqlError> {
    match resolution.is_finite() && resolution >= 0.0 {
        true => Ok(()),
        false => Err(CommunitySqlError::InvalidArgument(format!(
            "resolution must be a finite number of at least 0, not {resolution}"
        ))),
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
pub(super) mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::failure;

    /// Two triangles joined by one edge, undirected, as graph `tt`, with
    /// labels `halves` (one per triangle).
    pub(crate) fn two_triangles() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE tt_nodes (id bigint); CREATE TABLE tt_edges (src bigint, dst bigint);
             INSERT INTO tt_nodes SELECT generate_series(1, 6);
             INSERT INTO tt_edges VALUES (1, 2), (2, 3), (1, 3), (4, 5), (5, 6), (4, 6), (3, 4);
             SELECT warren.create_graph('tt', 'SELECT id FROM tt_nodes', 'SELECT src, dst FROM tt_edges',
                                        directed := false);
             SELECT warren.refresh('tt');
             SELECT warren.vector_from_query('tt', 'halves', 'SELECT id, (id - 1) / 3 FROM tt_nodes')",
        )
    }

    #[pg_test]
    fn modularity_scores_a_partition_as_networkx_does() -> Result<(), Error> {
        two_triangles()?;
        // m = 7; each triangle: 3 edges inside, degrees summing to 7.
        let q =
            Spi::get_one::<f64>("SELECT warren.modularity('tt', 'halves')")?.unwrap_or_default();
        assert!((q - 2.0 * (3.0 / 7.0 - 0.25)).abs() < 1e-15, "{q}");
        let half =
            Spi::get_one::<f64>("SELECT warren.modularity('tt', 'halves', resolution := 0.5)")?
                .unwrap_or_default();
        assert!((half - 2.0 * (3.0 / 7.0 - 0.125)).abs() < 1e-15, "{half}");
        Ok(())
    }

    #[pg_test]
    fn an_undirected_self_loop_counts_once() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE sl_nodes (id bigint); CREATE TABLE sl_edges (src bigint, dst bigint, w float8);
             INSERT INTO sl_nodes VALUES (1), (2); INSERT INTO sl_edges VALUES (1, 1, 2), (1, 2, 1);
             SELECT warren.create_graph('sl', 'SELECT id FROM sl_nodes',
                                        'SELECT src, dst, w FROM sl_edges',
                                        directed := false, weighted := true);
             SELECT warren.refresh('sl');
             SELECT warren.vector_from_query('sl', 'alone', 'SELECT id, id FROM sl_nodes')",
        )?;
        // k = (3, 1), 2m = 4: 2/4 − (3/4)² − (1/4)².
        let q = Spi::get_one::<f64>("SELECT warren.modularity('sl', 'alone')")?;
        assert_eq!(q, Some(-0.125));
        Ok(())
    }

    #[pg_test]
    fn modularity_refuses_bad_arguments() -> Result<(), Error> {
        two_triangles()?;
        Spi::run(
            "SELECT warren.vector_from_query('tt', 'half', 'SELECT id, id / 2.0 FROM tt_nodes')",
        )?;
        assert_eq!(
            failure("warren.modularity('tt', 'half')")?.as_deref(),
            Some("22023: labels: the label of dense id 0 is 0.5, which is not an integer")
        );
        assert_eq!(
            failure("warren.modularity('tt', 'halves', resolution := -1)")?.as_deref(),
            Some("22023: resolution must be a finite number of at least 0, not -1")
        );
        assert_eq!(
            failure("warren.modularity('tt', NULL)")?.as_deref(),
            Some("22004: labels must not be NULL")
        );
        Ok(())
    }
}
