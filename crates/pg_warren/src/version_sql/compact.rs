//! `warren.compact`: renumbers a graph's current version into a new id space
//! without tombstones, as a new, promoted version (DESIGN.md §5.2).

use pgrx::prelude::*;
use warren_core::graph::ExternalId;
use warren_core::version::compact as renumber;

use super::error::VersionOperationError;
use crate::access::require_owner;
use crate::build::{Catalog, SpiCatalog};
use crate::cache::load;
use crate::catalog::{version_record, GraphDefinition, NewVersion, VersionId, VersionStats};
use crate::extension::or_raise;
use crate::segment::{write_version, SegmentStore, SpiSegmentStore, CHUNK_SIZE};
use crate::source::with_id_type;

/// `warren.compact`: renumbers graph `name`'s current version to remove its
/// tombstones, records the result as a new version with `compacted_from`
/// set, promotes it (applying retention) and returns its id. The caller
/// must be a member of the owner role.
#[pg_extern(schema = "warren")]
fn compact(name: &str) -> i64 {
    or_raise(compact_graph(name))
}

fn compact_graph(name: &str) -> Result<VersionId, VersionOperationError> {
    let mut graph = GraphDefinition::find(name)?;
    require_owner(graph.owner, &graph.name, "compact")?;
    graph.lock()?;
    let source = graph
        .current_version
        .ok_or_else(|| VersionOperationError::NoCurrentVersion(graph.name.clone()))?;
    let (carried, built_lsn) = version_record(source)?;
    with_id_type!(graph.id_type, E => compact_version::<E>(
        &graph,
        source,
        &carried,
        CHUNK_SIZE,
        &mut SpiSegmentStore,
        &mut SpiCatalog::at_lsn(built_lsn),
    ))
}

/// Compacts version `source` of `graph` into a new, promoted version.
/// Its dropped and deduplicated edge counts are carried over from
/// `carried`, the source's recorded statistics: the compacted version holds
/// the same edges.
pub(crate) fn compact_version<E: ExternalId>(
    graph: &GraphDefinition,
    source: VersionId,
    carried: &VersionStats,
    chunk_size: usize,
    segments: &mut impl SegmentStore,
    catalog: &mut impl Catalog,
) -> Result<VersionId, VersionOperationError> {
    let loaded = load::<E>(segments, source)?;
    let compaction = renumber(&loaded.version()?)?;
    let stats = VersionStats::measure(
        &compaction.id_map,
        &compaction.graph,
        carried.dropped_edges,
        carried.deduplicated_edges,
    );
    let version = catalog.insert_version(&NewVersion {
        graph_id: graph.graph_id,
        stats,
        label: None,
        compacted_from: Some(source),
    })?;
    let bytes = write_version(
        segments,
        version,
        &compaction.id_map,
        &loaded.relations,
        &compaction.graph,
        chunk_size,
    )?;
    catalog.record_bytes(version, bytes)?;
    catalog.promote(graph, version)?;
    Ok(version)
}

#[cfg(test)]
mod unit {
    use super::*;
    use crate::build::MemoryCatalog;
    use crate::cache::load;
    use crate::catalog::{GraphDefinition, NewVersion, VersionStats};
    use crate::segment::{write_version, MemorySegmentStore};
    use pgrx::pg_sys;
    use warren_core::graph::{EdgeIntake, GraphCsr, IdMap, RelationDictionary};

    fn graph() -> GraphDefinition {
        GraphDefinition {
            graph_id: 1,
            name: "g".to_owned(),
            owner: pg_sys::Oid::INVALID,
            readers: Vec::new(),
            id_type: crate::source::IdType::Int8,
            node_query: String::new(),
            edge_query: String::new(),
            directed: true,
            weighted: true,
            has_relations: false,
            build_reverse: true,
            retain_versions: 2,
            current_version: Some(1),
        }
    }

    #[test]
    fn compaction_records_a_promoted_version_in_a_new_id_space() {
        // 10, 20 (tombstoned), 30, 40; edges 10 → 30, 40 → 10.
        let id_map = IdMap::<i64>::new()
            .rebuild(vec![10, 20, 30, 40])
            .and_then(|m| m.rebuild(vec![10, 30, 40]))
            .unwrap();
        let mut relations = RelationDictionary::new();
        let relation = relations.intern("").unwrap();
        let mut intake = EdgeIntake::directed(&id_map, true);
        intake.push(relation, Some(&10), Some(&30), 2.0).unwrap();
        intake.push(relation, Some(&40), Some(&10), 3.0).unwrap();
        let built = GraphCsr::build(intake.finish().0, 1, 4, true).unwrap();
        let mut store = MemorySegmentStore::default();
        let mut catalog = MemoryCatalog::default();
        let carried =
            VersionStats { dropped_edges: 3, deduplicated_edges: 2, ..VersionStats::default() };
        let source = catalog_version(&mut catalog, carried);
        write_version(&mut store, source, &id_map, &relations, &built, 16).unwrap();

        let compacted =
            compact_version::<i64>(&graph(), source, &carried, 16, &mut store, &mut catalog)
                .unwrap();

        let row = catalog.version(compacted).clone();
        assert_eq!(row.compacted_from, Some(source));
        assert_eq!(
            row.stats,
            VersionStats {
                node_count: 3,
                id_space: 3,
                edge_count: 2,
                dropped_edges: 3,
                deduplicated_edges: 2,
                target_width: 4,
            }
        );
        assert!(!row.staged);
        assert_eq!(catalog.current_version, Some(compacted));
        let loaded = load::<i64>(&mut store, compacted).unwrap();
        assert_eq!(loaded.id_map.dense_id(&40), Some(2));
        assert_eq!(loaded.id_map.tombstones().count(), 0);
        assert_eq!(loaded.graph.all_relations().forward().neighbour_weights(2), Some(&[3.0][..]));
        assert_eq!(loaded.relations, relations);
    }

    fn catalog_version(catalog: &mut MemoryCatalog, stats: VersionStats) -> i64 {
        use crate::build::Catalog;
        catalog
            .insert_version(&NewVersion { graph_id: 1, stats, label: None, compacted_from: None })
            .unwrap()
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{current, refresh, ring, versions};

    /// Graph `c`: a ring of 5, built, then node 3 removed and rebuilt, so the
    /// current version has a tombstone. Returns both versions.
    fn tombstoned() -> Result<(i64, i64), Error> {
        ring("c", 5, 3)?;
        let first = refresh("c", true)?;
        Spi::run("DELETE FROM c_nodes WHERE id = 3; DELETE FROM c_edges WHERE 3 IN (src, dst)")?;
        Ok((first, refresh("c", true)?))
    }

    fn row(version: i64) -> Result<Option<String>, Error> {
        Spi::get_one::<String>(&format!(
            "SELECT format('%s|%s|%s|%s|%s', compacted_from, staged, node_count, id_space,
                           edge_count)
             FROM warren.versions WHERE version_id = {version}"
        ))
    }

    #[pg_test]
    fn compact_promotes_a_version_in_a_new_id_space() -> Result<(), Error> {
        let (_, second) = tombstoned()?;
        let compacted = Spi::get_one::<i64>("SELECT warren.compact('c')")?;
        let compacted = compacted.ok_or(Error::InvalidPosition)?;
        assert_eq!(row(compacted)?, Some(format!("{second}|f|4|4|3")));
        assert_eq!(current("c")?, Some(compacted));
        // Later builds continue in the new id space.
        Spi::run("INSERT INTO c_nodes VALUES (9)")?;
        let next = refresh("c", true)?;
        assert_eq!(row(next)?, Some("|f|5|5|3".to_owned()));
        Ok(())
    }

    #[pg_test]
    fn the_version_a_compaction_came_from_can_still_be_dropped() -> Result<(), Error> {
        let (first, second) = tombstoned()?;
        let compacted = Spi::get_one::<i64>("SELECT warren.compact('c')")?.unwrap_or_default();
        Spi::run(&format!("SELECT warren.drop_version('c', {second})"))?;
        assert_eq!(versions("c")?, Some(format!("{first}:false {compacted}:false")));
        // compacted_from is now NULL; diff still refuses, because warren-core
        // finds dense ids that name different nodes.
        let refused = Spi::get_one::<bool>(&format!(
            "DO $$ BEGIN PERFORM warren.diff('c', {first}, {compacted}); RAISE 'diffed';
             EXCEPTION WHEN invalid_parameter_value THEN NULL; END $$; SELECT true"
        ));
        assert_eq!(refused, Ok(Some(true)));
        Ok(())
    }

    #[pg_test(error = "graph \"c\" has no current version to compact")]
    fn a_graph_never_promoted_cannot_be_compacted() -> Result<(), Error> {
        ring("c", 5, 2)?;
        Spi::run("SELECT warren.compact('c')")
    }

    #[pg_test(error = "must be a member of role \"c_owner\" to compact graph \"c\"")]
    fn only_the_owner_may_compact() -> Result<(), Error> {
        tombstoned()?;
        Spi::run("CREATE ROLE compacter; SET ROLE compacter; SELECT warren.compact('c')")
    }
}
