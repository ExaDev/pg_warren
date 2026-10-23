//! `warren.compact`: renumbers a graph's current version into a new id space
//! without tombstones, as a new, promoted version (DESIGN.md §5.2).

use pgrx::prelude::*;
use warren_core::graph::{BuildMemory, NodeOrder};
use warren_core::version::compact as renumber;

use super::error::VersionOperationError;
use crate::access::{entry, require_owner};
use crate::build::{max_build_memory, Catalog, SpiCatalog};
use crate::cache::{load, CachedIds};
use crate::catalog::{
    parse_node_order, version_record, GraphDefinition, NewVersion, VersionId, VersionStats,
};
use crate::segment::{write_version, SegmentStore, SpiSegmentStore, CHUNK_SIZE};
use crate::source::with_id_type;
use crate::vector::{move_vectors, moved_notice, SpiVectorStore, VectorStore};

/// `warren.compact`: renumbers graph `name`'s current version to remove its
/// tombstones, in `node_order` (`'id'` or `'locality'`; NULL is the graph's
/// own, DESIGN.md §14 item 22), records the result as a new version with
/// `compacted_from` set, promotes it (applying retention) and returns its
/// id. A `node_order` given becomes the graph's. The caller must be a
/// member of the owner role.
#[pg_extern(schema = "warren")]
fn compact(name: Option<&str>, node_order: default!(Option<&str>, "NULL")) -> i64 {
    entry(|_| compact_graph(name.ok_or(VersionOperationError::NullArgument("name"))?, node_order))
}

fn compact_graph(name: &str, node_order: Option<&str>) -> Result<VersionId, VersionOperationError> {
    let node_order = node_order.map(parse_node_order).transpose()?;
    let mut graph = GraphDefinition::find(name)?;
    require_owner(graph.owner, &graph.name, "compact")?;
    graph.require_query_source("compact")?;
    graph.lock("compact")?;
    graph.require_sound_current_version()?;
    if let Some(order) = node_order.filter(|&order| order != graph.node_order) {
        graph.set_node_order(order)?;
    }
    let source = graph
        .current_version
        .ok_or_else(|| VersionOperationError::NoCurrentVersion(graph.name.clone()))?;
    let (carried, built_lsn) = version_record(source)?;
    let (version, moved) = with_id_type!(graph.id_type, E => compact_version::<E>(
        &graph,
        source,
        &carried,
        &CompactionOrder { order: graph.node_order, memory: &BuildMemory::new(max_build_memory()) },
        CHUNK_SIZE,
        &mut Stores {
            segments: &mut SpiSegmentStore,
            vectors: &mut SpiVectorStore,
            catalog: &mut SpiCatalog::at_lsn(built_lsn),
        },
    ))?;
    if !moved.is_empty() {
        notice!("{}", moved_notice(&moved, source, version));
    }
    Ok(version)
}

/// The stores a compaction reads and writes.
pub(crate) struct Stores<'a, S, V, C> {
    /// Segments of both versions.
    pub(crate) segments: &'a mut S,
    /// Vectors bound to both versions.
    pub(crate) vectors: &'a mut V,
    /// The version catalog.
    pub(crate) catalog: &'a mut C,
}

/// The node order of a compaction, and the build memory its scratch is
/// charged to.
pub(crate) struct CompactionOrder<'a> {
    /// The order of the new id space.
    pub(crate) order: NodeOrder,
    /// `warren.max_build_memory`.
    pub(crate) memory: &'a BuildMemory,
}

/// Compacts version `source` of `graph` into a new, promoted version in
/// `ordering`. Its dropped and deduplicated edge counts are carried over
/// from `carried`, the source's recorded statistics: the compacted version
/// holds the same edges. The source's persisted vectors move to it, with
/// their rows in the new dense ids. Returns the version and the vectors
/// moved.
pub(crate) fn compact_version<E: CachedIds>(
    graph: &GraphDefinition,
    source: VersionId,
    carried: &VersionStats,
    ordering: &CompactionOrder<'_>,
    chunk_size: usize,
    stores: &mut Stores<'_, impl SegmentStore, impl VectorStore, impl Catalog>,
) -> Result<(VersionId, Vec<String>), VersionOperationError> {
    let Stores { segments, vectors, catalog } = stores;
    let loaded = load::<E>(*segments, source, graph.store_merged)?;
    let compaction = renumber(&loaded.version()?, ordering.order, ordering.memory)?;
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
        lineage: None,
    })?;
    let bytes = write_version(
        *segments,
        version,
        &compaction.id_map,
        &loaded.relations,
        &compaction.graph,
        chunk_size,
    )?;
    catalog.record_contents(version, &stats, bytes)?;
    let rows = compaction.renumbering.old_dense_ids();
    let moved = move_vectors(*vectors, graph.graph_id, source, version, rows, chunk_size)?;
    // With its vectors moved, retention may delete the source on promotion.
    catalog.promote(graph, version)?;
    Ok((version, moved))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod unit {
    use super::*;
    use crate::build::MemoryCatalog;
    use crate::cache::load;
    use crate::catalog::{GraphDefinition, NewVersion, VersionStats};
    use crate::segment::{write_version, MemorySegmentStore};
    use crate::vector::MemoryVectorStore;
    use pgrx::pg_sys;
    use warren_core::graph::{EdgeIntake, FixedKeys, GraphCsr, IdMap, RelationDictionary};

    fn graph() -> GraphDefinition {
        GraphDefinition {
            graph_id: 1,
            name: "g".to_owned(),
            source: crate::catalog::GraphSource::Query,
            owner: pg_sys::Oid::INVALID,
            readers: Vec::new(),
            id_type: crate::source::IdType::Int8,
            node_query: String::new(),
            edge_query: String::new(),
            search_path: String::new(),
            directed: true,
            weighted: true,
            has_relations: false,
            build_reverse: true,
            store_merged: true,
            retain_versions: 2,
            node_order: warren_core::graph::NodeOrder::Id,
            current_version: Some(1),
            stray_current_version: None,
        }
    }

    #[test]
    fn compaction_records_a_promoted_version_in_a_new_id_space() {
        // 10, 20 (tombstoned), 30, 40; edges 10 → 30, 40 → 10.
        let id_map = IdMap::<FixedKeys<i64>>::new()
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

        let mut vectors = MemoryVectorStore::default();
        let mut stores =
            Stores { segments: &mut store, vectors: &mut vectors, catalog: &mut catalog };
        let ordering = CompactionOrder { order: NodeOrder::Id, memory: &BuildMemory::unlimited() };
        let compacted = compact_version::<FixedKeys<i64>>(
            &graph(),
            source,
            &carried,
            &ordering,
            16,
            &mut stores,
        )
        .unwrap()
        .0;

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
        let loaded = load::<FixedKeys<i64>>(&mut store, compacted, true).unwrap();
        assert_eq!(loaded.id_map.dense_id(&40), Some(2));
        assert_eq!(loaded.id_map.tombstones().count(), 0);
        assert_eq!(loaded.graph.relations()[0].forward().neighbour_weights(2), Some(&[3.0][..]));
        assert_eq!(loaded.relations, relations);
    }

    fn catalog_version(catalog: &mut MemoryCatalog, stats: VersionStats) -> i64 {
        use crate::build::Catalog;
        catalog
            .insert_version(&NewVersion {
                graph_id: 1,
                stats,
                label: None,
                compacted_from: None,
                lineage: None,
            })
            .unwrap()
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{current, failure, refresh, ring, rows, versions};

    /// DESIGN.md §5.1: compact must not copy another graph's version (or
    /// none) into this graph.
    #[pg_test]
    fn compact_refuses_a_stray_current_version() -> Result<(), Error> {
        use crate::fixture::{set_current_unchecked, stray_current_failure, two_graphs};
        let (_, other) = two_graphs("cstray")?;
        for stray in [other, -1] {
            set_current_unchecked("cstray_a", stray)?;
            assert_eq!(
                failure("warren.compact('cstray_a')")?,
                Some(stray_current_failure("cstray_a", stray))
            );
        }
        Ok(())
    }

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
    fn compact_moves_persisted_vectors_by_node_with_their_creators() -> Result<(), Error> {
        let (_, second) = tombstoned()?;
        Spi::run(
            "CREATE ROLE c_reader; SELECT warren.grant('c', 'c_reader'); GRANT SELECT ON c_nodes TO c_reader;
             SELECT warren.vector_dense_id('c', 'id', 'f64');
             SET ROLE c_reader;
             SELECT warren.vector_from_query('c', 'emb', 'SELECT id, ARRAY[id, -id] FROM c_nodes', 'f32');
             SELECT warren.vector_const('c', 'scratch', 1, persist := false);
             RESET ROLE",
        )?;
        let compacted = Spi::get_one::<i64>("SELECT warren.compact('c')")?.unwrap_or_default();
        let carried = rows(&format!(
            "SELECT name || ':' || dtype || ':' || width || ':' || length || ':' || created_by
             FROM warren.vectors WHERE version_id = {compacted} ORDER BY name"
        ))?;
        let user = Spi::get_one::<String>("SELECT current_user::text")?.unwrap_or_default();
        assert_eq!(carried, Some(format!("emb:f32:2:4:c_reader id:f64:1:4:{user}")));
        // Each node keeps its value: `id` holds the old dense ids.
        assert_eq!(
            rows(
                "SELECT node_id || ':' || value FROM warren.vector_values('c', 'id', NULL::bigint)"
            )?
            .as_deref(),
            Some("1:0 2:1 4:3 5:4")
        );
        assert_eq!(
            rows("SELECT node_id || ':' || row::text FROM warren.matrix_rows('c', 'emb', NULL::bigint)")?
                .as_deref(),
            Some("1:{1,-1} 2:{2,-2} 4:{4,-4} 5:{5,-5}")
        );
        let dense = rows(&format!(
            "SELECT string_agg(encode(data, 'hex'), '') FROM warren.vector_chunks
             JOIN warren.vectors USING (vector_id) WHERE version_id = {compacted} AND name = 'id'"
        ))?;
        assert_eq!(
            dense.as_deref(),
            Some("0000000000000000000000000000f03f00000000000008400000000000001040"),
            "rows in the new dense ids, without the tombstone"
        );
        // They moved: the source has none left (so retention can drop it),
        // and transient vectors are not carried.
        let kept = Spi::get_one::<i64>(&format!(
            "SELECT count(*) FROM warren.vectors WHERE version_id = {second}"
        ))?;
        assert_eq!(kept, Some(0));
        let scratch = failure("warren.vector_values('c', 'scratch', NULL::bigint)")?;
        assert!(scratch.unwrap_or_default().starts_with("42704"));
        Ok(())
    }

    #[pg_test]
    fn after_moving_its_vectors_retention_drops_the_compacted_source() -> Result<(), Error> {
        ring("k", 5, 1)?;
        refresh("k", true)?;
        Spi::run("DELETE FROM k_nodes WHERE id = 3; DELETE FROM k_edges WHERE 3 IN (src, dst)")?;
        refresh("k", true)?;
        Spi::run("SELECT warren.vector_const('k', 'rank', 1)")?;
        let compacted = Spi::get_one::<i64>("SELECT warren.compact('k')")?.unwrap_or_default();
        assert_eq!(versions("k")?, Some(format!("{compacted}:false")));
        assert_eq!(
            rows("SELECT node_id || ':' || value FROM warren.vector_values('k', 'rank', NULL::bigint)")?
                .as_deref(),
            Some("1:1 2:1 4:1 5:1")
        );
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

    #[pg_test]
    fn a_version_older_than_a_compaction_cannot_be_promoted() -> Result<(), Error> {
        let (_, second) = tombstoned()?;
        let staged = refresh("c", false)?;
        Spi::run("SELECT warren.compact('c')")?;
        assert_eq!(state(&format!("warren.promote('c', {staged})"))?.as_deref(), Some("55000"));
        assert_eq!(state(&format!("warren.promote('c', {second})"))?.as_deref(), Some("55000"));
        Ok(())
    }

    #[pg_test]
    fn a_refresh_does_not_build_on_a_version_older_than_a_compaction() -> Result<(), Error> {
        let (_, second) = tombstoned()?;
        Spi::run("SELECT warren.compact('c')")?;
        Spi::run(&format!("UPDATE warren.graphs SET current_version = {second}"))?;
        assert_eq!(state("warren.refresh('c')")?.as_deref(), Some("55000"));
        Ok(())
    }

    /// The SQLSTATE `expression` fails with.
    fn state(expression: &str) -> Result<Option<String>, Error> {
        Ok(failure(expression)?.map(|failure| failure[..5].to_owned()))
    }

    #[pg_test]
    fn the_superseding_compaction_is_named() -> Result<(), Error> {
        ring("c", 5, 3)?;
        refresh("c", true)?;
        let staged = refresh("c", false)?;
        let compacted = Spi::get_one::<i64>("SELECT warren.compact('c')")?.unwrap_or_default();
        assert_eq!(
            failure(&format!("warren.promote('c', {staged})"))?,
            Some(format!(
                "55000: version {staged} of graph \"c\" is in an id space that compacted \
                 version {compacted} replaced"
            ))
        );
        Ok(())
    }

    /// Each live node's dense id in graph `name`'s current version, as
    /// `node:dense` text by node.
    fn dense_ids(name: &str) -> Result<Option<String>, Error> {
        rows(&format!(
            "SELECT node_id || ':' || dense_id FROM warren.id_map('{name}', NULL::bigint)
             WHERE live ORDER BY node_id"
        ))
    }

    fn node_order(name: &str) -> Result<Option<String>, Error> {
        Spi::get_one::<String>(&format!(
            "SELECT node_order FROM warren.graphs WHERE name = '{name}'"
        ))
    }

    /// DESIGN.md §14 item 22: `compact(…, node_order := …)` orders the new
    /// id space and records the order, which later compactions follow.
    #[pg_test]
    fn compact_in_locality_order_orders_the_ids_and_records_the_order() -> Result<(), Error> {
        // The live graph is the path 4 → 5 → 1 → 2.
        tombstoned()?;
        Spi::run("SELECT warren.compact('c', node_order := 'locality')")?;
        assert_eq!(node_order("c")?.as_deref(), Some("locality"));
        // Reverse Cuthill-McKee from 2 (its end of least degree with the
        // lower id in id order): 2 1 5 4, reversed.
        assert_eq!(dense_ids("c")?.as_deref(), Some("1:2 2:3 4:0 5:1"));
        // NULL is the graph's order, now locality: the ids stay.
        Spi::run("SELECT warren.compact('c', NULL)")?;
        assert_eq!(dense_ids("c")?.as_deref(), Some("1:2 2:3 4:0 5:1"));
        Spi::run("SELECT warren.compact('c', 'id')")?;
        assert_eq!(node_order("c")?.as_deref(), Some("id"));
        assert_eq!(dense_ids("c")?.as_deref(), Some("1:0 2:1 4:2 5:3"));
        Ok(())
    }

    #[pg_test]
    fn compact_refuses_a_null_name() -> Result<(), Error> {
        assert_eq!(
            failure("warren.compact(NULL)")?.as_deref(),
            Some("22004: name must not be NULL")
        );
        Ok(())
    }

    #[pg_test]
    fn compact_refuses_an_unknown_node_order() -> Result<(), Error> {
        let (_, second) = tombstoned()?;
        assert_eq!(
            failure("warren.compact('c', 'nearby')")?.as_deref(),
            Some("22023: node_order must be 'id' or 'locality', not 'nearby'")
        );
        assert_eq!(current("c")?, Some(second));
        assert_eq!(node_order("c")?.as_deref(), Some("id"));
        Ok(())
    }

    /// The locality order's scratch is charged to warren.max_build_memory.
    #[pg_test]
    fn a_locality_compaction_over_the_build_memory_budget_is_refused() -> Result<(), Error> {
        ring("big", 70_000, 2)?;
        let built = refresh("big", true)?;
        Spi::run("SET LOCAL warren.max_build_memory = '1MB'")?;
        let refused = failure("warren.compact('big', 'locality')")?.unwrap_or_default();
        assert!(refused.starts_with("53400: build memory limit exceeded"), "{refused}");
        assert_eq!(current("big")?, Some(built));
        assert_eq!(node_order("big")?.as_deref(), Some("id"));
        Ok(())
    }

    #[pg_test]
    fn a_vector_moves_by_node_through_a_locality_compaction() -> Result<(), Error> {
        tombstoned()?;
        Spi::run(
            "SELECT warren.vector_from_query('c', 'tenfold', 'SELECT id, id * 10.0 FROM c_nodes');
             SELECT warren.compact('c', 'locality')",
        )?;
        assert_eq!(
            rows("SELECT node_id || ':' || value FROM warren.vector_values('c', 'tenfold', NULL::bigint)
                  ORDER BY node_id")?
            .as_deref(),
            Some("1:10 2:20 4:40 5:50")
        );
        Ok(())
    }
}
