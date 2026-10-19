//! The build pipeline (DESIGN.md §5.4 steps 2–6), over the build's ports:
//! stable-id merge against the base version's id map, edge intake, CSR
//! build, chunked segment write, version row and optional promotion. Graph
//! logic is `warren_core::graph`'s; this module only sequences it.

use warren_core::graph::{
    AnyCsr, EdgeIntake, ExternalId, GraphCsr, IdMap, RelationCsr, RelationDictionary,
    RelationIndex, Tombstones,
};

use super::error::BuildError;
use super::source::{EdgeRow, SourceReader};
use super::version::Catalog;
use crate::catalog::{GraphDefinition, VersionId, VersionStats};
use crate::segment::{
    csr_segment, read_segment, write_segment, CsrPart, CsrScope, Direction, SegmentStore, ID_MAP,
    RELATION_DICTIONARY, TOMBSTONES,
};

/// The name of the one relation of a graph without relations.
const IMPLICIT_RELATION: &str = "";

/// What a refresh asks for, beyond the graph itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RefreshRequest<'a> {
    /// Whether to make the new version current.
    pub(crate) promote: bool,
    /// The version's label.
    pub(crate) label: Option<&'a str>,
    /// The segment chunk size in bytes.
    pub(crate) chunk_size: usize,
}

/// Builds a new version of `graph` from its sources and returns its id.
/// Dense ids continue from the graph's current version, if it has one.
pub(crate) fn build<E: ExternalId>(
    graph: &GraphDefinition,
    request: &RefreshRequest<'_>,
    source: &mut impl SourceReader<E>,
    segments: &mut impl SegmentStore,
    catalog: &mut impl Catalog,
) -> Result<VersionId, BuildError> {
    let (base, mut relations) = match graph.current_version {
        Some(version) => base_version(segments, version)?,
        None => (IdMap::new(), RelationDictionary::new()),
    };
    let id_map = base.rebuild(source.node_ids()?)?;
    drop(base);
    let (csr, dropped_edges) = adjacency(graph, &id_map, &mut relations, source)?;
    let stats = VersionStats {
        node_count: id_map.node_count(),
        id_space: id_map.id_space(),
        edge_count: csr.relations().iter().map(|r| r.forward().edge_count()).sum(),
        dropped_edges,
        deduplicated_edges: csr.deduplicated_edges(),
        target_width: match csr.all_relations().forward() {
            AnyCsr::U32(_) => 4,
            AnyCsr::U64(_) => 8,
        },
    };
    let version = catalog.insert_version(graph.graph_id, &stats, request.label)?;
    let mut writer = SegmentWriter { segments, version, chunk_size: request.chunk_size, bytes: 0 };
    writer.write(ID_MAP, &id_map.encode()?)?;
    writer.write(TOMBSTONES, &id_map.tombstones().encode()?)?;
    writer.write(RELATION_DICTIONARY, &relations.encode()?)?;
    for (index, relation) in csr.relations().iter().enumerate() {
        writer.write_csrs(CsrScope::Relation(index), relation)?;
    }
    if let Some(merged) = csr.merged() {
        writer.write_csrs(CsrScope::Merged, merged)?;
    }
    catalog.record_bytes(version, writer.bytes)?;
    if request.promote {
        catalog.promote(graph, version)?;
    }
    Ok(version)
}

/// The id map and relation dictionary of the version a build starts from.
fn base_version<E: ExternalId>(
    segments: &mut impl SegmentStore,
    version: VersionId,
) -> Result<(IdMap<E>, RelationDictionary), BuildError> {
    let tombstones = Tombstones::decode(&read_segment(segments, version, TOMBSTONES)?)?;
    let id_map = IdMap::decode(&read_segment(segments, version, ID_MAP)?, tombstones)?;
    let relations =
        RelationDictionary::decode(&read_segment(segments, version, RELATION_DICTIONARY)?)?;
    Ok((id_map, relations))
}

/// Reads the edge rows into the CSRs, interning new relations. Returns the
/// CSRs and the number of dropped edges: rows with a NULL relation or
/// weight the graph needs, or a NULL, unknown or tombstoned endpoint.
fn adjacency<E: ExternalId>(
    graph: &GraphDefinition,
    id_map: &IdMap<E>,
    relations: &mut RelationDictionary,
    source: &mut impl SourceReader<E>,
) -> Result<(GraphCsr, u64), BuildError> {
    let implicit = match graph.has_relations {
        true => None,
        false => Some(relations.intern(IMPLICIT_RELATION)?),
    };
    let mut intake = match graph.directed {
        true => EdgeIntake::directed(id_map, graph.weighted),
        false => EdgeIntake::undirected(id_map, graph.weighted),
    };
    let mut incomplete_rows = 0;
    source.edges(&mut |row: EdgeRow<'_, E>| {
        let Some((relation, weight)) = relation_and_weight(graph, implicit, relations, &row)?
        else {
            incomplete_rows += 1;
            return Ok(());
        };
        Ok(intake.push(relation, row.source.as_ref(), row.target.as_ref(), weight)?)
    })?;
    let (edges, dropped_endpoints) = intake.finish();
    let csr = GraphCsr::build(edges, relations.len(), id_map.id_space(), graph.build_reverse)?;
    Ok((csr, dropped_endpoints + incomplete_rows))
}

/// The relation and weight of an edge row, or `None` if a field the graph
/// needs is NULL.
fn relation_and_weight<E>(
    graph: &GraphDefinition,
    implicit: Option<RelationIndex>,
    relations: &mut RelationDictionary,
    row: &EdgeRow<'_, E>,
) -> Result<Option<(RelationIndex, f64)>, BuildError> {
    let relation = match (implicit, row.relation) {
        (Some(relation), _) => relation,
        (None, Some(name)) => relations.intern(name)?,
        (None, None) => return Ok(None),
    };
    let weight = match (graph.weighted, row.weight) {
        (false, _) => 1.0,
        (true, Some(weight)) => weight,
        (true, None) => return Ok(None),
    };
    Ok(Some((relation, weight)))
}

/// Writes the segments of one version, counting their bytes.
struct SegmentWriter<'s, S> {
    segments: &'s mut S,
    version: VersionId,
    chunk_size: usize,
    bytes: u64,
}

impl<S: SegmentStore> SegmentWriter<'_, S> {
    fn write(&mut self, name: &str, bytes: &[u8]) -> Result<(), BuildError> {
        self.bytes += write_segment(self.segments, self.version, name, bytes, self.chunk_size)?;
        Ok(())
    }

    /// The forward and reverse CSRs of a relation, or of the merged CSR.
    fn write_csrs(&mut self, scope: CsrScope, csrs: &RelationCsr) -> Result<(), BuildError> {
        self.write_csr(Direction::Forward, scope, csrs.forward())?;
        if let Some(reverse) = csrs.reverse() {
            self.write_csr(Direction::Reverse, scope, reverse)?;
        }
        Ok(())
    }

    fn write_csr(
        &mut self,
        direction: Direction,
        scope: CsrScope,
        csr: &AnyCsr,
    ) -> Result<(), BuildError> {
        let encoded = csr.encode()?;
        self.write(&csr_segment(direction, CsrPart::Offsets, scope), &encoded.offsets)?;
        self.write(&csr_segment(direction, CsrPart::Targets, scope), &encoded.targets)?;
        if let Some(weights) = &encoded.weights {
            self.write(&csr_segment(direction, CsrPart::Weights, scope), weights)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::source::tests::{MemorySource, OwnedEdgeRow};
    use crate::build::version::tests::MemoryCatalog;
    use crate::catalog::GraphDefinition;
    use crate::catalog::VersionId;
    use crate::segment::{read_segment, MemorySegmentStore};
    use pgrx::pg_sys;
    use warren_core::graph::{AnyCsr, EdgeIntake, GraphCsr, IdMap, RelationDictionary, Tombstones};

    fn graph() -> GraphDefinition {
        GraphDefinition {
            graph_id: 1,
            name: "g".to_owned(),
            owner: pg_sys::Oid::INVALID,
            readers: Vec::new(),
            node_query: String::new(),
            edge_query: String::new(),
            directed: true,
            weighted: false,
            has_relations: false,
            build_reverse: true,
            retain_versions: 2,
            current_version: None,
        }
    }

    fn edge(source: i64, target: i64) -> OwnedEdgeRow {
        (Some(source), Some(target), None, None)
    }

    fn typed_edge(source: i64, target: i64, relation: &str, weight: f64) -> OwnedEdgeRow {
        (Some(source), Some(target), Some(relation.to_owned()), Some(weight))
    }

    /// A graph, its segment store and its catalog of versions.
    #[derive(Default)]
    struct World {
        store: MemorySegmentStore,
        catalog: MemoryCatalog,
    }

    impl World {
        fn refresh(
            &mut self,
            graph: &GraphDefinition,
            source: &MemorySource,
            promote: bool,
        ) -> VersionId {
            let request = RefreshRequest { promote, label: Some("l"), chunk_size: 16 };
            let graph =
                GraphDefinition { current_version: self.catalog.current_version, ..graph.clone() };
            build(&graph, &request, &mut source.clone(), &mut self.store, &mut self.catalog)
                .unwrap()
        }

        fn segment(&mut self, version: VersionId, name: &str) -> Vec<u8> {
            read_segment(&mut self.store, version, name).unwrap()
        }

        fn has_segment(&mut self, version: VersionId, name: &str) -> bool {
            read_segment(&mut self.store, version, name).is_ok()
        }

        fn id_map(&mut self, version: VersionId) -> IdMap<i64> {
            let tombstones = Tombstones::decode(&self.segment(version, "tombstones")).unwrap();
            IdMap::decode(&self.segment(version, "id_map"), tombstones).unwrap()
        }

        fn relations(&mut self, version: VersionId) -> RelationDictionary {
            RelationDictionary::decode(&self.segment(version, "rel_dict")).unwrap()
        }

        /// The CSR stored under `prefix` (`""` or `"rev_"`) and `scope`.
        fn csr(&mut self, version: VersionId, prefix: &str, scope: &str) -> AnyCsr {
            let id_space = self.catalog.version(version).stats.id_space;
            let offsets = self.segment(version, &format!("{prefix}offsets:{scope}"));
            let targets = self.segment(version, &format!("{prefix}targets:{scope}"));
            let weights_name = format!("{prefix}weights:{scope}");
            let weights = self
                .has_segment(version, &weights_name)
                .then(|| self.segment(version, &weights_name));
            AnyCsr::decode(id_space, &offsets, &targets, weights.as_deref()).unwrap()
        }

        fn stored_bytes(&self, version: VersionId) -> u64 {
            self.store
                .chunks
                .iter()
                .filter(|((v, _, _), _)| *v == version)
                .map(|(_, data)| data.len() as u64)
                .sum()
        }
    }

    fn neighbours(csr: &AnyCsr, id_map: &IdMap<i64>, external: i64) -> Vec<i64> {
        let dense = id_map.dense_id(&external).unwrap();
        csr.neighbours(dense).map(|d| id_map.external_id(d).unwrap()).collect()
    }

    #[test]
    fn a_first_build_records_a_promoted_version_with_its_stats() {
        let mut world = World::default();
        let source = MemorySource {
            nodes: vec![4, 1, 3, 2, 3],
            edges: vec![
                edge(1, 2),
                edge(2, 3),
                edge(2, 3),
                edge(3, 99),
                (None, Some(1), None, None),
            ],
        };
        let version = world.refresh(&graph(), &source, true);
        let row = world.catalog.version(version).clone();
        assert_eq!(
            row.stats,
            VersionStats {
                node_count: 4,
                id_space: 4,
                edge_count: 2,
                dropped_edges: 2,
                deduplicated_edges: 1,
                target_width: 4,
            }
        );
        assert_eq!(row.graph_id, 1);
        assert_eq!(row.label.as_deref(), Some("l"));
        assert!(!row.staged);
        assert_eq!(world.catalog.current_version, Some(version));
        assert_eq!(row.bytes, world.stored_bytes(version));
    }

    #[test]
    fn a_staged_build_leaves_the_current_version_alone() {
        let mut world = World::default();
        let source = MemorySource { nodes: vec![1], edges: vec![] };
        let first = world.refresh(&graph(), &source, true);
        let staged = world.refresh(&graph(), &source, false);
        assert!(world.catalog.version(staged).staged);
        assert_eq!(world.catalog.current_version, Some(first));
    }

    #[test]
    fn segments_span_several_chunks_and_decode_to_the_graph_built_directly() {
        let mut world = World::default();
        let nodes: Vec<i64> = (0..40).map(|n| n * 10).collect();
        let edges: Vec<(i64, i64)> = (0..40).map(|n| (n * 10, (n * 70) % 400)).collect();
        let source = MemorySource {
            nodes: nodes.clone(),
            edges: edges.iter().map(|&(s, t)| edge(s, t)).collect(),
        };
        let version = world.refresh(&graph(), &source, true);
        assert!(world.store.chunks.contains_key(&(version, "id_map".to_owned(), 2)));

        let id_map = IdMap::new().rebuild(nodes).unwrap();
        let mut relations = RelationDictionary::new();
        let relation = relations.intern("").unwrap();
        let mut intake = EdgeIntake::directed(&id_map, false);
        for (s, t) in &edges {
            intake.push(relation, Some(s), Some(t), 1.0).unwrap();
        }
        let expected = GraphCsr::build(intake.finish().0, 1, id_map.id_space(), true).unwrap();

        assert_eq!(world.id_map(version), id_map);
        assert_eq!(world.relations(version), relations);
        let only = &expected.relations()[0];
        assert_eq!(&world.csr(version, "", "0"), only.forward());
        assert_eq!(Some(&world.csr(version, "rev_", "0")), only.reverse());
        assert!(!world.has_segment(version, "offsets:merged"));
    }

    #[test]
    fn dense_ids_are_stable_across_builds() {
        let mut world = World::default();
        let first =
            MemorySource { nodes: vec![10, 20, 30], edges: vec![edge(10, 20), edge(20, 30)] };
        let v1 = world.refresh(&graph(), &first, true);
        let second = MemorySource {
            nodes: vec![5, 10, 30, 40],
            edges: vec![edge(10, 20), edge(10, 30), edge(40, 5)],
        };
        let v2 = world.refresh(&graph(), &second, true);

        let (before, after) = (world.id_map(v1), world.id_map(v2));
        for survivor in [10, 30] {
            assert_eq!(after.dense_id(&survivor), before.dense_id(&survivor));
        }
        assert_eq!(after.dense_id(&20), None);
        assert!(after.tombstones().contains(1));
        assert_eq!(after.external_id(1), Some(20));
        assert_eq!(after.dense_id(&5), Some(3));
        assert_eq!(after.dense_id(&40), Some(4));

        let stats = world.catalog.version(v2).stats;
        assert_eq!((stats.node_count, stats.id_space, stats.edge_count), (4, 5, 2));
        assert_eq!(stats.dropped_edges, 1, "the edge to the tombstoned node is dropped");
        let forward = world.csr(v2, "", "0");
        assert_eq!(neighbours(&forward, &after, 10), [30]);
        assert_eq!(neighbours(&forward, &after, 40), [5]);
        assert_eq!(forward.degree(1), 0, "a tombstoned node's row is empty");
    }

    #[test]
    fn a_build_starts_from_the_current_version_not_a_staged_one() {
        let mut world = World::default();
        let v1 = world.refresh(&graph(), &MemorySource { nodes: vec![10], edges: vec![] }, true);
        world.refresh(&graph(), &MemorySource { nodes: vec![30], edges: vec![] }, false);
        let v3 =
            world.refresh(&graph(), &MemorySource { nodes: vec![10, 20], edges: vec![] }, true);
        assert_eq!(world.id_map(v1).dense_id(&10), Some(0));
        assert_eq!(world.id_map(v3).dense_id(&20), Some(1));
        assert_eq!(world.catalog.version(v3).stats.id_space, 2);
    }

    #[test]
    fn relations_get_a_csr_each_plus_the_merged_csr_with_weights() {
        let mut world = World::default();
        let graph = GraphDefinition { weighted: true, has_relations: true, ..graph() };
        let source = MemorySource {
            nodes: vec![1, 2, 3],
            edges: vec![
                typed_edge(1, 2, "b", 1.5),
                typed_edge(1, 2, "a", 2.0),
                typed_edge(1, 3, "a", 1.0),
                typed_edge(1, 3, "a", 4.0),
                (Some(2), Some(3), None, Some(1.0)),
                (Some(2), Some(3), Some("a".to_owned()), None),
            ],
        };
        let version = world.refresh(&graph, &source, true);
        let stats = world.catalog.version(version).stats;
        assert_eq!((stats.edge_count, stats.dropped_edges, stats.deduplicated_edges), (3, 2, 1));
        assert_eq!(world.relations(version).names(), ["b", "a"]);

        let id_map = world.id_map(version);
        let a = world.csr(version, "", "1");
        assert_eq!(neighbours(&a, &id_map, 1), [2, 3]);
        assert_eq!(a.neighbour_weights(0), Some(&[2.0, 5.0][..]));
        let merged = world.csr(version, "", "merged");
        assert_eq!(merged.neighbour_weights(0), Some(&[3.5, 5.0][..]));
        let reverse_b = world.csr(version, "rev_", "0");
        assert_eq!(neighbours(&reverse_b, &id_map, 2), [1]);
        assert!(world.has_segment(version, "rev_weights:merged"));
    }

    #[test]
    fn a_later_build_keeps_the_relation_indexes() {
        let mut world = World::default();
        let graph = GraphDefinition { has_relations: true, ..graph() };
        let row = |relation: &str| typed_edge(1, 2, relation, 0.0);
        world.refresh(
            &graph,
            &MemorySource { nodes: vec![1, 2], edges: vec![row("b"), row("a")] },
            true,
        );
        let v2 = world.refresh(
            &graph,
            &MemorySource { nodes: vec![1, 2], edges: vec![row("c"), row("a")] },
            true,
        );
        assert_eq!(world.relations(v2).names(), ["b", "a", "c"]);
        assert_eq!(world.csr(v2, "", "0").edge_count(), 0);
    }

    #[test]
    fn undirected_edges_are_stored_in_both_directions_without_reverse_segments() {
        let mut world = World::default();
        let graph = GraphDefinition { directed: false, build_reverse: false, ..graph() };
        let source = MemorySource { nodes: vec![1, 2], edges: vec![edge(1, 2), edge(2, 2)] };
        let version = world.refresh(&graph, &source, true);
        let id_map = world.id_map(version);
        let forward = world.csr(version, "", "0");
        assert_eq!(neighbours(&forward, &id_map, 1), [2]);
        assert_eq!(neighbours(&forward, &id_map, 2), [1, 2]);
        assert_eq!(world.catalog.version(version).stats.edge_count, 3);
        assert!(!world.has_segment(version, "rev_offsets:0"));
    }

    #[test]
    fn a_graph_with_relations_but_no_edges_stores_only_the_merged_csr() {
        let mut world = World::default();
        let graph = GraphDefinition { has_relations: true, ..graph() };
        let version = world.refresh(&graph, &MemorySource { nodes: vec![1], edges: vec![] }, true);
        assert!(world.relations(version).is_empty());
        assert_eq!(world.csr(version, "", "merged").id_space(), 1);
    }
}
