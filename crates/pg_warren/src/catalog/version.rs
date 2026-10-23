//! Version rows: `warren.versions`, and promotion through
//! `warren.graphs.current_version` (DESIGN.md §5.1, §7). Written as the
//! catalog owner.

use pgrx::prelude::*;
use warren_core::graph::{GraphCsr, IdKeys, IdMap, TargetWidth};

use super::retention::{retention, Kept, VersionSummary};
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
    pub(crate) fn measure<K: IdKeys>(
        id_map: &IdMap<K>,
        graph: &GraphCsr,
        dropped_edges: u64,
        deduplicated_edges: u64,
    ) -> Self {
        let edge_count = graph.relations().iter().map(|r| r.forward().edge_count()).sum();
        Self::new(id_map, edge_count, dropped_edges, deduplicated_edges)
    }

    /// The statistics of a version with this id map, `edge_count` edges
    /// over its relations, and the given dropped and deduplicated edge
    /// counts.
    pub(crate) fn new<K: IdKeys>(
        id_map: &IdMap<K>,
        edge_count: u64,
        dropped_edges: u64,
        deduplicated_edges: u64,
    ) -> Self {
        Self {
            node_count: id_map.node_count(),
            id_space: id_map.id_space(),
            edge_count,
            dropped_edges,
            deduplicated_edges,
            // Every CSR of a version has the width its id space implies.
            target_width: match TargetWidth::for_id_space(id_map.id_space()) {
                TargetWidth::U32 => 4,
                TargetWidth::U64 => 8,
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
    /// The version it was contracted from, for a contraction.
    pub(crate) lineage: Option<Lineage>,
}

/// What a version derived by contraction was contracted from (DESIGN.md
/// §6.6): `versions.parent_version` and `versions.label_vector`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Lineage {
    /// The parent version.
    pub(crate) parent_version: VersionId,
    /// The persisted label vector it was contracted on (`None` for a
    /// transient one).
    pub(crate) label_vector: Option<i64>,
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
                             deduplicated_edges, target_width, bytes, label, compacted_from,
                             parent_version, label_vector)
                         SELECT $1, coalesce(max(seq), 0) + 1, now(), $2::pg_lsn, $3, $4,
                                $5, $6, $7, $8, 0, $9, $10, $11,
                                -- Locked, so a concurrent delete of the
                                -- vector waits, and then its trigger clears
                                -- the link; one deleted already gives NULL.
                                (SELECT x.vector_id FROM warren.vectors x
                                 WHERE x.vector_id = $12 FOR KEY SHARE)
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
                            new.lineage.map(|l| l.parent_version).into(),
                            new.lineage.and_then(|l| l.label_vector).into(),
                        ],
                    )?
                    .first()
                    .get_one::<i64>()?,
            )
        })
    })?;
    version.ok_or(CatalogError::Spi(pgrx::spi::Error::InvalidPosition))
}

/// Records a version's statistics and the total size of its segments,
/// once they are written: a build streams its segments under the version
/// before it knows its edge counts.
pub(crate) fn record_contents(
    version: VersionId,
    stats: &VersionStats,
    bytes: u64,
) -> Result<(), CatalogError> {
    as_catalog_owner(|| {
        Spi::connect_mut(|client| {
            client.update(
                "UPDATE warren.versions
                 SET node_count = $2, id_space = $3, edge_count = $4, dropped_edges = $5,
                     deduplicated_edges = $6, target_width = $7, bytes = $8
                 WHERE version_id = $1",
                None,
                &[
                    version.into(),
                    bigint(stats.node_count, "node_count")?.into(),
                    bigint(stats.id_space, "id_space")?.into(),
                    bigint(stats.edge_count, "edge_count")?.into(),
                    bigint(stats.dropped_edges, "dropped_edges")?.into(),
                    bigint(stats.deduplicated_edges, "deduplicated_edges")?.into(),
                    stats.target_width.into(),
                    bigint(bytes, "bytes")?.into(),
                ],
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
                             WHERE x.version_id = v.version_id),
                            (SELECT count(*) FROM warren.versions d
                             WHERE d.parent_version = v.version_id)
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
                        derived_versions: row.get(5)?.unwrap_or_default(),
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
    for kept in outcome.kept {
        ereport!(
            NOTICE,
            PgSqlErrorCode::ERRCODE_SUCCESSFUL_COMPLETION,
            kept_notice(&kept, &graph.name)
        );
    }
    Ok(())
}

/// The `NOTICE` for version `kept` of `graph`, kept beyond retention
/// because vectors are bound to it or versions were contracted from it.
fn kept_notice(kept: &Kept, graph: &str) -> String {
    let vectors = match kept.vectors {
        0 => None,
        1 => Some("1 vector is bound to it".to_owned()),
        n => Some(format!("{n} vectors are bound to it")),
    };
    let derived = match kept.derived_versions {
        0 => None,
        1 => Some("1 contracted version derives from it".to_owned()),
        n => Some(format!("{n} contracted versions derive from it")),
    };
    let reasons: Vec<String> = vectors.into_iter().chain(derived).collect();
    format!(
        "version {} of graph \"{graph}\" is kept beyond retain_versions: {}",
        kept.version,
        reasons.join(", and ")
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

/// The version a contraction's version was contracted from (DESIGN.md
/// §6.6), and its graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Parent {
    /// The parent version.
    pub(crate) version: VersionId,
    /// Its graph's id.
    pub(crate) graph_id: i32,
    /// Its graph's name.
    pub(crate) graph: String,
}

/// The version `version` was contracted from, if it was and that version
/// still exists (dropping it sets `parent_version` to NULL).
pub(crate) fn parent_of(version: VersionId) -> Result<Option<Parent>, CatalogError> {
    as_catalog_owner(|| {
        Spi::connect(|client| {
            let row = client
                .select(
                    "SELECT p.version_id, g.graph_id, g.name
                     FROM warren.versions v
                     JOIN warren.versions p ON p.version_id = v.parent_version
                     JOIN warren.graphs g ON g.graph_id = p.graph_id
                     WHERE v.version_id = $1",
                    Some(1),
                    &[version.into()],
                )?
                .first();
            if row.is_empty() {
                return Ok(None);
            }
            Ok(Some(Parent {
                version: row.get(1)?.unwrap_or_default(),
                graph_id: row.get(2)?.unwrap_or_default(),
                graph: row.get(3)?.unwrap_or_default(),
            }))
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

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::{insert_version, Lineage, NewVersion, VersionStats};
    use crate::fixture::{refresh, ring};

    /// A version records its label vector only if that vector still
    /// exists: a vector deleted before the insert is not linked.
    #[pg_test]
    fn a_label_vector_gone_before_the_insert_is_not_recorded() -> Result<(), Error> {
        ring("lv", 3, 2)?;
        let parent = refresh("lv", true)?;
        let graph = Spi::get_one::<i32>("SELECT graph_id FROM warren.graphs WHERE name = 'lv'")?
            .ok_or(Error::InvalidPosition)?;
        let insert = |label_vector| {
            insert_version(
                &NewVersion {
                    graph_id: graph,
                    stats: VersionStats { target_width: 4, ..VersionStats::default() },
                    label: None,
                    compacted_from: None,
                    lineage: Some(Lineage { parent_version: parent, label_vector }),
                },
                "0/0",
            )
            .map_err(|_| Error::InvalidPosition)
        };
        let gone = insert(Some(i64::MAX))?;
        let recorded = Spi::get_one::<i64>(&format!(
            "SELECT label_vector FROM warren.versions WHERE version_id = {gone}"
        ))?;
        assert_eq!(recorded, None);
        let vector = Spi::get_one::<i64>("SELECT warren.vector_const('lv', 'l', 1)")?;
        let kept = insert(vector)?;
        let recorded = Spi::get_one::<i64>(&format!(
            "SELECT label_vector FROM warren.versions WHERE version_id = {kept}"
        ))?;
        assert_eq!(recorded, vector);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod notices {
    use super::{kept_notice, Kept};

    fn kept(vectors: i64, derived_versions: i64) -> Kept {
        Kept { version: 3, vectors, derived_versions }
    }

    #[test]
    fn the_retention_notice_counts_vectors_in_the_singular_and_plural() {
        assert_eq!(
            kept_notice(&kept(1, 0), "g"),
            "version 3 of graph \"g\" is kept beyond retain_versions: 1 vector is bound to it"
        );
        assert_eq!(
            kept_notice(&kept(2, 0), "g"),
            "version 3 of graph \"g\" is kept beyond retain_versions: 2 vectors are bound to it"
        );
    }

    #[test]
    fn the_retention_notice_counts_contracted_versions() {
        assert_eq!(
            kept_notice(&kept(0, 1), "g"),
            "version 3 of graph \"g\" is kept beyond retain_versions: \
             1 contracted version derives from it"
        );
        assert_eq!(
            kept_notice(&kept(1, 2), "g"),
            "version 3 of graph \"g\" is kept beyond retain_versions: 1 vector is bound to it, \
             and 2 contracted versions derive from it"
        );
    }
}
