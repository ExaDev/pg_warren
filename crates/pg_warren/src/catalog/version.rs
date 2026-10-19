//! Version rows: `warren.versions`, and promotion through
//! `warren.graphs.current_version` (DESIGN.md §5.1, §7). Written as the
//! extension owner.

use pgrx::prelude::*;

use super::retention::{retention, VersionSummary};
use super::{CatalogError, GraphDefinition};
use crate::access::as_extension_owner;

/// A version id: `warren.versions.version_id`.
pub(crate) type VersionId = i64;

/// The statistics of a built version, as recorded in `warren.versions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VersionStats {
    /// Live nodes.
    pub(crate) node_count: u64,
    /// The dense id high-water mark, tombstones included.
    pub(crate) id_space: u64,
    /// Stored edges summed over relations, after deduplication. An
    /// undirected edge is stored, and counted, in both directions.
    pub(crate) edge_count: u64,
    /// Edge rows dropped for a NULL field or an unknown or tombstoned
    /// endpoint.
    pub(crate) dropped_edges: u64,
    /// Parallel edges folded by deduplication.
    pub(crate) deduplicated_edges: u64,
    /// The CSR target width in bytes: 4 or 8.
    pub(crate) target_width: i16,
}

/// The current WAL position, as `pg_lsn` text, for `versions.built_lsn`.
pub(crate) fn current_wal_lsn() -> Result<String, CatalogError> {
    Ok(Spi::get_one::<String>("SELECT pg_current_wal_lsn()::text")?.unwrap_or_default())
}

/// Inserts a staged version of graph `graph_id` with the next sequence
/// number and `bytes` 0, and returns its id.
pub(crate) fn insert_version(
    graph_id: i32,
    stats: &VersionStats,
    label: Option<&str>,
    built_lsn: &str,
) -> Result<VersionId, CatalogError> {
    let version = as_extension_owner(|| {
        Spi::connect_mut(|client| {
            Ok::<_, CatalogError>(
                client
                    .update(
                        "INSERT INTO warren.versions (graph_id, seq, built_at, built_lsn,
                             node_count, id_space, edge_count, dropped_edges,
                             deduplicated_edges, target_width, bytes, label)
                         SELECT $1, coalesce(max(seq), 0) + 1, now(), $2::pg_lsn, $3, $4,
                                $5, $6, $7, $8, 0, $9
                         FROM warren.versions WHERE graph_id = $1
                         RETURNING version_id",
                        None,
                        &[
                            graph_id.into(),
                            built_lsn.into(),
                            bigint(stats.node_count, "node_count")?.into(),
                            bigint(stats.id_space, "id_space")?.into(),
                            bigint(stats.edge_count, "edge_count")?.into(),
                            bigint(stats.dropped_edges, "dropped_edges")?.into(),
                            bigint(stats.deduplicated_edges, "deduplicated_edges")?.into(),
                            stats.target_width.into(),
                            label.into(),
                        ],
                    )?
                    .first()
                    .get_one::<i64>()?,
            )
        })
    })?;
    version.ok_or(CatalogError::Spi(pgrx::spi::Error::InvalidPosition))
}

/// Records the total size of a version's segments.
pub(crate) fn record_bytes(version: VersionId, bytes: u64) -> Result<(), CatalogError> {
    as_extension_owner(|| {
        Spi::connect_mut(|client| {
            client.update(
                "UPDATE warren.versions SET bytes = $2 WHERE version_id = $1",
                None,
                &[version.into(), bigint(bytes, "bytes")?.into()],
            )?;
            Ok(())
        })
    })
}

/// Makes `version` the current version of `graph`, no longer staged, then
/// deletes the promoted versions retention no longer keeps, raising a
/// `NOTICE` for each version kept only because vectors are bound to it.
pub(crate) fn promote_version(
    graph: &GraphDefinition,
    version: VersionId,
) -> Result<(), CatalogError> {
    let outcome = as_extension_owner(|| {
        Spi::connect_mut(|client| {
            client.update(
                "WITH promoted AS (
                     UPDATE warren.versions SET staged = false WHERE version_id = $1)
                 UPDATE warren.graphs SET current_version = $1 WHERE graph_id = $2",
                None,
                &[version.into(), graph.graph_id.into()],
            )?;
            let versions = client
                .select(
                    "SELECT v.version_id, v.seq, v.staged,
                            (SELECT count(*) FROM warren.vectors x
                             WHERE x.version_id = v.version_id)
                     FROM warren.versions v WHERE v.graph_id = $1",
                    None,
                    &[graph.graph_id.into()],
                )?
                .map(|row| {
                    Ok(VersionSummary {
                        version: row.get(1)?.unwrap_or_default(),
                        seq: row.get(2)?.unwrap_or_default(),
                        staged: row.get(3)?.unwrap_or_default(),
                        bound_vectors: row.get(4)?.unwrap_or_default(),
                    })
                })
                .collect::<Result<Vec<_>, pgrx::spi::Error>>()?;
            let outcome = retention(&versions, version, graph.retain_versions);
            client.update(
                "DELETE FROM warren.versions WHERE version_id = ANY ($1)",
                None,
                &[outcome.delete.clone().into()],
            )?;
            Ok::<_, CatalogError>(outcome)
        })
    })?;
    for (kept, vectors) in outcome.kept_for_vectors {
        ereport!(
            NOTICE,
            PgSqlErrorCode::ERRCODE_SUCCESSFUL_COMPLETION,
            format!(
                "version {kept} of graph \"{}\" is kept beyond retain_versions: {vectors} \
                 vectors are bound to it",
                graph.name
            )
        );
    }
    Ok(())
}

/// Checks that `version` is a version of `graph`.
pub(crate) fn require_version(
    graph: &GraphDefinition,
    version: VersionId,
) -> Result<(), CatalogError> {
    let found = as_extension_owner(|| {
        Ok::<_, CatalogError>(Spi::get_one_with_args::<bool>(
            "SELECT EXISTS (SELECT 1 FROM warren.versions WHERE version_id = $1 AND graph_id = $2)",
            &[version.into(), graph.graph_id.into()],
        )?)
    })?;
    match found {
        Some(true) => Ok(()),
        _ => Err(CatalogError::VersionNotFound { graph: graph.name.clone(), version }),
    }
}

/// Deletes `version` of `graph` with its segments and bound vectors. The
/// current version cannot be dropped.
pub(crate) fn drop_version(
    graph: &GraphDefinition,
    version: VersionId,
) -> Result<(), CatalogError> {
    require_version(graph, version)?;
    if graph.current_version == Some(version) {
        return Err(CatalogError::DropCurrent { graph: graph.name.clone(), version });
    }
    as_extension_owner(|| {
        Spi::connect_mut(|client| {
            client.update(
                "DELETE FROM warren.versions WHERE version_id = $1",
                None,
                &[version.into()],
            )?;
            Ok(())
        })
    })
}

/// `count` as a `bigint` column value.
fn bigint(count: u64, column: &'static str) -> Result<i64, CatalogError> {
    i64::try_from(count).map_err(|_| CatalogError::TooLarge(column))
}
