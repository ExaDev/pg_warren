//! `warren.vector_reduce` and `warren.vector_norm_diff` (DESIGN.md §6.3,
//! §7).

use pgrx::prelude::*;
use warren_core::compute::{norm_diff, reduce_live, Norm, Reduction};
use warren_core::version;

use super::vectors::Vectors;
use super::{entry, ComputeSqlError};
use crate::cache::resolve;
use crate::catalog::compaction_between;
use crate::interrupts::workers;
use crate::interrupts::PostgresInterrupts;
use crate::source::with_id_type;

/// `warren.vector_reduce`: `op` (`sum`, `max`, `min`, `l1`, `l2` or `nnz`)
/// over every element of the live rows of vector `x`, read as `double
/// precision`; NULL for the `max` or `min` of no elements.
#[pg_extern(schema = "warren", stable, parallel_restricted)]
fn vector_reduce(
    graph: Option<&str>,
    x: Option<&str>,
    op: Option<&str>,
    version: default!(Option<i64>, "NULL"),
) -> Option<f64> {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let x = x.ok_or(ComputeSqlError::NullArgument("x"))?;
        let reduction: Reduction = op.ok_or(ComputeSqlError::NullArgument("op"))?.parse()?;
        let resolved = resolve(graph, version, "read")?;
        with_id_type!(resolved.graph.id_type, E => {
            let mut vectors = Vectors::<E>::open(resolved)?;
            let vector = vectors.get(x)?;
            let tombstones = vectors.tombstones();
            Ok(reduce_live(&*workers()?, &mut PostgresInterrupts, &vector, tombstones, reduction)?)
        })
    })
}

/// `warren.vector_norm_diff`: `norm(a − b)` (`l1`, `l2` or `linf`) for
/// vector `a` at `a_version` and vector `b` at `b_version` (each the
/// current version if NULL), aligned by dense id: a row past the end of
/// the shorter vector counts as zero (DESIGN.md §7). Versions are
/// comparable only where their shared dense ids name the same nodes: not
/// on either side of a compaction, nor sibling staged versions whose new
/// nodes diverge (DESIGN.md §8).
#[pg_extern(schema = "warren", stable, parallel_restricted)]
fn vector_norm_diff(
    graph: Option<&str>,
    a: Option<&str>,
    b: Option<&str>,
    norm: default!(Option<&str>, "'l2'"),
    a_version: default!(Option<i64>, "NULL"),
    b_version: default!(Option<i64>, "NULL"),
) -> f64 {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let a = a.ok_or(ComputeSqlError::NullArgument("a"))?;
        let b = b.ok_or(ComputeSqlError::NullArgument("b"))?;
        let norm: Norm = norm.ok_or(ComputeSqlError::NullArgument("norm"))?.parse()?;
        let a_side = resolve(graph, a_version, "read")?;
        let b_side = resolve(graph, b_version, "read")?;
        if let Some((compacted, _)) =
            compaction_between(&a_side.graph, a_side.version, b_side.version)?
        {
            return Err(ComputeSqlError::InvalidArgument(format!(
                "versions {} and {} of graph \"{graph}\" are in different id spaces: version \
                 {compacted} was compacted between them",
                a_side.version, b_side.version
            )));
        }
        with_id_type!(a_side.graph.id_type, E => {
            let (mut a_vectors, mut b_vectors) = Vectors::<E>::open_pair(a_side, b_side)?;
            version::shared_dense_ids(&a_vectors.loaded.id_map, &b_vectors.loaded.id_map)?;
            let (a, b) = (a_vectors.get(a)?, b_vectors.get(b)?);
            Ok(norm_diff(&*workers()?, &mut PostgresInterrupts, &a, &b, norm)?)
        })
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, refresh, small};

    #[pg_test]
    fn reductions_match_sql_aggregates_over_live_nodes() -> Result<(), Error> {
        small("t")?;
        Spi::run("DELETE FROM t_nodes WHERE id = 1")?;
        refresh("t", true)?;
        Spi::run("SELECT warren.vector_dense_id('t', 'x', 'f64')")?;
        let found = Spi::get_one::<bool>(
            "SELECT warren.vector_reduce('t', 'x', 'sum') = sum(value)
                AND warren.vector_reduce('t', 'x', 'min') = min(value)
                AND warren.vector_reduce('t', 'x', 'max') = max(value)
                AND warren.vector_reduce('t', 'x', 'l1') = sum(abs(value))
                AND warren.vector_reduce('t', 'x', 'l2') = sqrt(sum(value * value))
                AND warren.vector_reduce('t', 'x', 'nnz') = count(*) FILTER (WHERE value <> 0)
             FROM warren.vector_values('t', 'x', NULL::bigint)",
        )?;
        assert_eq!(found, Some(true));
        assert_eq!(Spi::get_one::<f64>("SELECT warren.vector_reduce('t', 'x', 'min')")?, Some(1.0));
        let unknown = failure("warren.vector_reduce('t', 'x', 'avg')")?;
        assert_eq!(
            unknown.as_deref(),
            Some("22023: unknown reduction \"avg\": expected sum, max, min, l1, l2 or nnz")
        );
        Ok(())
    }

    #[pg_test]
    fn norm_diff_aligns_vectors_of_two_versions_by_dense_id() -> Result<(), Error> {
        let first = small("t")?;
        Spi::run("SELECT warren.vector_dense_id('t', 'old', 'f64')")?;
        Spi::run("INSERT INTO t_nodes VALUES (7), (8)")?;
        refresh("t", true)?;
        Spi::run("SELECT warren.vector_const('t', 'new', 1)")?;
        // new = 1 at dense ids 0..8; old = 0..5 at 0..6 and nothing at 6, 7.
        let l1 = Spi::get_one::<f64>(&format!(
            "SELECT warren.vector_norm_diff('t', 'new', 'old', 'l1', b_version := {first})"
        ))?;
        assert_eq!(l1, Some(1.0 + 0.0 + 1.0 + 2.0 + 3.0 + 4.0 + 1.0 + 1.0));
        let linf = Spi::get_one::<f64>(&format!(
            "SELECT warren.vector_norm_diff('t', 'old', 'new', 'linf', {first})"
        ))?;
        assert_eq!(linf, Some(4.0));
        Ok(())
    }

    #[pg_test]
    fn norm_diff_refuses_sibling_staged_versions() -> Result<(), Error> {
        // Two staged builds from one current version give the new nodes 9
        // and 10 the same dense id, 6. Like diff and distance, norm_diff
        // refuses to align them.
        small("t")?;
        Spi::run("INSERT INTO t_nodes VALUES (9)")?;
        let candidate = refresh("t", false)?;
        Spi::run("DELETE FROM t_nodes WHERE id = 9; INSERT INTO t_nodes VALUES (10)")?;
        let sibling = refresh("t", false)?;
        for version in [candidate, sibling] {
            Spi::run(&format!(
                "SELECT warren.vector_dense_id('t', 'd', 'f64', version := {version})"
            ))?;
        }
        let refused = failure(&format!(
            "warren.vector_norm_diff('t', 'd', 'd', 'l2', {candidate}, {sibling})"
        ))?;
        assert_eq!(
            refused.as_deref(),
            Some("22023: dense id 6 names different nodes in the two versions")
        );
        Ok(())
    }
}
