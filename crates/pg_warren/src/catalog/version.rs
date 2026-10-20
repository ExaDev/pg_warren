//! Version rows: `warren.versions`, and promotion through
//! `warren.graphs.current_version` (DESIGN.md §5.1, §7). Written as the
//! catalog owner.

use pgrx::prelude::*;
use warren_core::graph::{AnyCsr, ExternalId, GraphCsr, IdMap};

use super::retention::{retention, VersionSummary};
use super::{CatalogError, GraphDefinition};
use crate::access::as_catalog_owner;
use crate::read_only::read_one;

/// A version id: `warren.versions.version_id`.
pub(crate) type VersionId = i64;

/// The statistics of a built version, as recorded in `warren.versions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

impl VersionStats {
    /// The statistics of a version with this id map and these CSRs, and
    /// the given dropped and deduplicated edge counts.
    pub(crate) fn measure<E: ExternalId>(
        id_map: &IdMap<E>,
        graph: &GraphCsr,
        dropped_edges: u64,
        deduplicated_edges: u64,
    ) -> Self {
        Self {
            node_count: id_map.node_count(),
            id_space: id_map.id_space(),
            edge_count: graph.relations().iter().map(|r| r.forward().edge_count()).sum(),
            dropped_edges,
            deduplicated_edges,
            target_width: match graph.all_relations().forward() {
                AnyCsr::U32(_) => 4,
                AnyCsr::U64(_) => 8,
            },
        }
    }
}

/// A version to record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NewVersion<'a> {
    /// The graph it is a version of.
    pub(crate) graph_id: i32,
    /// Its statistics.
    pub(crate) stats: VersionStats,
    /// Its label.
    pub(crate) label: Option<&'a str>,
    /// The version it was compacted from, which starts a new id space.
    pub(crate) compacted_from: Option<VersionId>,
}

/// The current WAL position, as `pg_lsn` text, for `versions.built_lsn`.
pub(crate) fn current_wal_lsn() -> Result<String, CatalogError> {
    Ok(read_one::<String>("SELECT pg_current_wal_lsn()::text", &[])?.unwrap_or_default())
}

/// Inserts a staged version of graph `graph_id` with the next sequence
/// number and `bytes` 0, and returns its id.
pub(crate) fn insert_version(
    new: &NewVersion<'_>,
    built_lsn: &str,
) -> Result<VersionId, CatalogError> {
    let stats = &new.stats;
    let version = as_catalog_owner(|| {
        Spi::connect_mut(|client| {
            Ok::<_, CatalogError>(
                client
                    .update(
                        "INSERT INTO warren.versions (graph_id, seq, built_at, built_lsn,
                             node_count, id_space, edge_count, dropped_edges,
                             deduplicated_edges, target_width, bytes, label, compacted_from)
                         SELECT $1, coalesce(max(seq), 0) + 1, now(), $2::pg_lsn, $3, $4,
                                $5, $6, $7, $8, 0, $9, $10
                         FROM warren.versions WHERE graph_id = $1
                         RETURNING version_id",
                        None,
                        &[
                            new.graph_id.into(),
                            built_lsn.into(),
                            bigint(stats.node_count, "node_count")?.into(),
                            bigint(stats.id_space, "id_space")?.into(),
                            bigint(stats.edge_count, "edge_count")?.into(),
                            bigint(stats.dropped_edges, "dropped_edges")?.into(),
                            bigint(stats.deduplicated_edges, "deduplicated_edges")?.into(),
                            stats.target_width.into(),
                            new.label.into(),
                            new.compacted_from.into(),
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
    as_catalog_owner(|| {
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
    let outcome = as_catalog_owner(|| {
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
            kept_notice(kept, vectors, &graph.name)
        );
    }
    Ok(())
}

/// The `NOTICE` for version `kept` of `graph`, kept beyond retention
/// because `vectors` vectors are bound to it.
fn kept_notice(kept: VersionId, vectors: i64, graph: &str) -> String {
    let bound = match vectors {
        1 => "1 vector is".to_owned(),
        n => format!("{n} vectors are"),
    };
    format!(
        "version {kept} of graph \"{graph}\" is kept beyond retain_versions: {bound} bound to it"
    )
}

/// The recorded statistics of `version` and its `built_lsn` (as text).
pub(crate) fn version_record(version: VersionId) -> Result<(VersionStats, String), CatalogError> {
    as_catalog_owner(|| {
        Spi::connect(|client| {
            let row = client
                .select(
                    "SELECT node_count, id_space, edge_count, dropped_edges,
                            deduplicated_edges, target_width, built_lsn::text
                     FROM warren.versions WHERE version_id = $1",
                    Some(1),
                    &[version.into()],
                )?
                .first();
            let count = |column| -> Result<u64, CatalogError> {
                let value: Option<i64> = row.get(column)?;
                Ok(u64::try_from(value.unwrap_or_default()).unwrap_or_default())
            };
            Ok((
                VersionStats {
                    node_count: count(1)?,
                    id_space: count(2)?,
                    edge_count: count(3)?,
                    dropped_edges: count(4)?,
                    deduplicated_edges: count(5)?,
                    target_width: row.get(6)?.unwrap_or_default(),
                },
                row.get(7)?.unwrap_or_default(),
            ))
        })
    })
}

/// The first compaction between versions `a` and `b` of `graph`, in `seq`
/// order, as (compacted version, the version it was compacted from): a
/// compacted version starts a new id space, so it separates every earlier
/// version from itself and every later one.
pub(crate) fn compaction_between(
    graph: &GraphDefinition,
    a: VersionId,
    b: VersionId,
) -> Result<Option<(VersionId, VersionId)>, CatalogError> {
    as_catalog_owner(|| {
        Spi::connect(|client| {
            let row = client
                .select(
                    "SELECT c.version_id, c.compacted_from FROM warren.versions c,
                            (SELECT least(x.seq, y.seq) AS low, greatest(x.seq, y.seq) AS high
                             FROM warren.versions x, warren.versions y
                             WHERE x.version_id = $2 AND y.version_id = $3) r
                     WHERE c.graph_id = $1 AND c.seq > r.low AND c.seq <= r.high
                       AND c.compacted_from IS NOT NULL
                     ORDER BY c.seq LIMIT 1",
                    Some(1),
                    &[graph.graph_id.into(), a.into(), b.into()],
                )?
                .first();
            if row.is_empty() {
                return Ok(None);
            }
            Ok(row.get::<i64>(1)?.zip(row.get::<i64>(2)?))
        })
    })
}

/// Checks that `version` is in `graph`'s current id space: that no
/// compacted version has a higher `seq`. Promotion and refresh call it, so
/// a graph never returns to an id space a compaction replaced.
pub(crate) fn require_current_id_space(
    graph: &GraphDefinition,
    version: VersionId,
) -> Result<(), CatalogError> {
    let compacted = as_catalog_owner(|| {
        Spi::connect(|client| {
            let row = client
                .select(
                    "SELECT c.version_id FROM warren.versions c, warren.versions v
                     WHERE v.version_id = $2 AND c.graph_id = $1
                       AND c.compacted_from IS NOT NULL AND c.seq > v.seq
                     ORDER BY c.seq DESC LIMIT 1",
                    Some(1),
                    &[graph.graph_id.into(), version.into()],
                )?
                .first();
            match row.is_empty() {
                true => Ok::<_, CatalogError>(None),
                false => Ok(row.get::<i64>(1)?),
            }
        })
    })?;
    match compacted {
        None => Ok(()),
        Some(compacted) => {
            Err(CatalogError::SupersededIdSpace { graph: graph.name.clone(), version, compacted })
        }
    }
}

/// Checks that `version` is a version of `graph`.
pub(crate) fn require_version(
    graph: &GraphDefinition,
    version: VersionId,
) -> Result<(), CatalogError> {
    let found = as_catalog_owner(|| {
        Ok::<_, CatalogError>(read_one::<bool>(
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
    as_catalog_owner(|| {
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

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod notices {
    use super::kept_notice;

    #[test]
    fn the_retention_notice_counts_vectors_in_the_singular_and_plural() {
        assert_eq!(
            kept_notice(3, 1, "g"),
            "version 3 of graph \"g\" is kept beyond retain_versions: 1 vector is bound to it"
        );
        assert_eq!(
            kept_notice(3, 2, "g"),
            "version 3 of graph \"g\" is kept beyond retain_versions: 2 vectors are bound to it"
        );
    }
}
