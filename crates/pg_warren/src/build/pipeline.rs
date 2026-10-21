//! The build pipeline (DESIGN.md §5.4 steps 2–6), over the build's ports:
//! stable-id merge against the base version's id map, edge intake, the
//! locality order of a first build (§14 item 22), CSR build, chunked
//! segment write, version row and optional promotion. Graph logic is
//! `warren_core::graph`'s; this module only sequences it, with
//! every graph-sized buffer charged to one `warren.max_build_memory`
//! budget: a build that would exceed it is refused with 53400 before its
//! CSRs are built where the plan shows it, else where it reserves.

use warren_core::graph::{
    BuildCharge, BuildMemory, BuildPlan, CsrLayout, EdgeIntake, EdgeList, ExternalId, GraphCsr,
    GraphError, IdMap, Membership, NodeOrder, Permutation, RelationDictionary, RelationIndex,
};

use super::error::BuildError;
use super::source::{EdgeRow, EdgeSink, SourceReader};
use super::version::Catalog;
use crate::cache::{load_identity, Identity};
use crate::catalog::{GraphDefinition, Lineage, NewVersion, VersionId, VersionStats};
use crate::segment::{writer_memory, SegmentStore, SegmentWriter};

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
    /// The most bytes the build may hold (`warren.max_build_memory`).
    pub(crate) memory_budget: u64,
}

/// A version a refresh built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Built {
    /// The new version.
    pub(crate) version: VersionId,
    /// The most build memory it held at once.
    pub(crate) memory_peak: u64,
}

/// Builds a new version of `graph` from its sources. Dense ids continue
/// from the graph's current version, if it has one.
pub(crate) fn build<E: ExternalId>(
    graph: &GraphDefinition,
    request: &RefreshRequest<'_>,
    source: &mut impl SourceReader<E>,
    segments: &mut impl SegmentStore,
    catalog: &mut impl Catalog,
) -> Result<Built, BuildError> {
    let memory = BuildMemory::new(request.memory_budget);
    let base = match graph.current_version {
        Some(version) => load_identity(segments, version, &memory)?,
        None => Identity {
            id_map: IdMap::new(),
            id_map_charge: BuildCharge::none(&memory),
            relations: RelationDictionary::new(),
            relations_charge: BuildCharge::none(&memory),
        },
    };
    let Identity { id_map: base, id_map_charge: base_charge, relations, relations_charge } = base;
    let (mut id_map, _id_map_charge) = base.rebuild_within(source.node_ids(&memory)?)?;
    drop((base, base_charge));
    let layout = CsrLayout { build_reverse: graph.build_reverse, store_merged: graph.store_merged };
    let mut relations = Relations {
        dictionary: relations,
        charge: relations_charge,
        id_space: id_map.id_space(),
        layout,
        budget: request.memory_budget,
    };
    let implicit = match graph.has_relations {
        true => None,
        false => Some(relations.intern(IMPLICIT_RELATION)?),
    };
    relations.check_offsets()?;
    let (mut edges, dropped_edges) =
        edges(graph, &id_map, implicit, &mut relations, source, &memory)?;
    if graph.node_order == NodeOrder::Locality && graph.current_version.is_none() {
        // A first build: its ids are provisional until ordered (§14 item 22).
        let permutation = Permutation::locality(&edges, id_map.id_space(), &memory)?;
        edges.renumber(&permutation)?;
        id_map.renumber(&permutation, &memory)?;
    }
    let Relations { dictionary: relations, charge: _relations_charge, .. } = relations;
    let contents = Contents { id_map, relations, edges, dropped_edges };
    let version = write_contents(graph, request, contents, None, &memory, segments, catalog)?;
    Ok(Built { version, memory_peak: memory.peak() })
}

/// A new version's id map, relation dictionary and edges, as a build or a
/// contraction made them.
pub(crate) struct Contents<E> {
    /// The id map, tombstones included.
    pub(crate) id_map: IdMap<E>,
    /// The relation dictionary.
    pub(crate) relations: RelationDictionary,
    /// The edges, in dense ids.
    pub(crate) edges: EdgeList,
    /// The edge rows dropped while reading them.
    pub(crate) dropped_edges: u64,
}

/// What a version of a contraction was derived from (DESIGN.md §6.6).
pub(crate) struct Derivation<'m> {
    /// The parent version and the label vector.
    pub(crate) lineage: Lineage,
    /// Each parent node's community.
    pub(crate) membership: &'m Membership,
}

/// Writes a new version of `graph` from `contents` (DESIGN.md §5.4 steps
/// 4–6): plans its CSRs within `memory`, refused before anything is
/// written if they would not fit; records the version, staged; streams
/// its CSRs, then its identity segments and, for a contraction, its
/// `membership`, into `segments`; records its statistics and bytes; and
/// promotes it if asked.
pub(crate) fn write_contents<E: ExternalId>(
    graph: &GraphDefinition,
    request: &RefreshRequest<'_>,
    contents: Contents<E>,
    derivation: Option<Derivation<'_>>,
    memory: &BuildMemory,
    segments: &mut impl SegmentStore,
    catalog: &mut impl Catalog,
) -> Result<VersionId, BuildError> {
    let Contents { id_map, relations, edges, dropped_edges } = contents;
    let layout = CsrLayout { build_reverse: graph.build_reverse, store_merged: graph.store_merged };
    let beside = writer_memory(request.chunk_size);
    let plan = BuildPlan::new(&edges, relations.len(), id_map.id_space(), layout, memory, beside)?;
    memory.require(plan.bytes_needed() + beside)?;

    let stats = VersionStats::new(&id_map, 0, dropped_edges, 0);
    let version = catalog.insert_version(&NewVersion {
        graph_id: graph.graph_id,
        stats,
        label: request.label,
        compacted_from: None,
        lineage: derivation.as_ref().map(|derivation| derivation.lineage),
    })?;
    let mut writer = SegmentWriter::new(segments, version, request.chunk_size, memory)?;
    let built = plan.build(edges, &mut writer)?;
    writer.write_identity(&id_map, &relations)?;
    if let Some(derivation) = &derivation {
        writer.write_membership(derivation.membership)?;
    }
    let stats =
        VersionStats::new(&id_map, built.edge_count, dropped_edges, built.deduplicated_edges);
    catalog.record_contents(version, &stats, writer.bytes())?;
    if request.promote {
        catalog.promote(graph, version)?;
    }
    Ok(version)
}

/// Reads the edge rows into an edge list charged to `memory`, interning new
/// relations. Returns it, without spare capacity, and the number of
/// dropped edges: rows with a NULL relation or weight the graph needs, or a
/// NULL, unknown or tombstoned endpoint.
fn edges<E: ExternalId>(
    graph: &GraphDefinition,
    id_map: &IdMap<E>,
    implicit: Option<RelationIndex>,
    relations: &mut Relations,
    source: &mut impl SourceReader<E>,
    memory: &BuildMemory,
) -> Result<(EdgeList, u64), BuildError> {
    let intake = match graph.directed {
        true => EdgeIntake::directed(id_map, graph.weighted),
        false => EdgeIntake::undirected(id_map, graph.weighted),
    };
    let intake = intake.within(memory);
    let mut collector = EdgeCollector { graph, implicit, relations, intake, incomplete_rows: 0 };
    source.edges(&mut collector)?;
    let EdgeCollector { intake, incomplete_rows, .. } = collector;
    let (edges, dropped_endpoints) = intake.finish();
    Ok((edges, dropped_endpoints + incomplete_rows))
}

/// The relation dictionary of a build, charged to its memory as it grows,
/// and checked to keep the version's offsets within the budget whenever a
/// relation is added, so that neither waits for the whole edge query.
struct Relations {
    dictionary: RelationDictionary,
    charge: BuildCharge,
    id_space: u64,
    layout: CsrLayout,
    budget: u64,
}

impl Relations {
    /// The index of `name`, added if it is new.
    fn intern(&mut self, name: &str) -> Result<RelationIndex, BuildError> {
        let before = self.dictionary.len();
        let index = self.dictionary.intern_within(name, &mut self.charge)?;
        if self.dictionary.len() > before {
            self.check_offsets()?;
        }
        Ok(index)
    }

    /// Refuses a version whose offsets would exceed the budget.
    fn check_offsets(&self) -> Result<(), GraphError> {
        GraphCsr::check_offsets(self.dictionary.len(), self.id_space, self.layout, self.budget)
    }
}

/// The edge sink of a build: remaps rows through the edge intake.
struct EdgeCollector<'b, 'm, E> {
    graph: &'b GraphDefinition,
    /// The one relation of a graph without relations.
    implicit: Option<RelationIndex>,
    relations: &'b mut Relations,
    intake: EdgeIntake<'m, E>,
    /// Rows dropped for a NULL relation or weight.
    incomplete_rows: u64,
}

impl<E: ExternalId> EdgeSink<E> for EdgeCollector<'_, '_, E> {
    /// Reserves the edge list for `rows` up front, so that it is not grown
    /// geometrically past its final size. A reservation that fails (an
    /// overestimate, or one past the build's budget) is ignored: the list
    /// then grows as rows arrive, as far as the budget allows.
    fn expect_rows(&mut self, rows: u64) {
        if let Ok(rows) = usize::try_from(rows) {
            let _ = self.intake.reserve(rows);
        }
    }

    fn row(&mut self, row: EdgeRow<'_, E>) -> Result<(), BuildError> {
        let Some((relation, weight)) =
            relation_and_weight(self.graph, self.implicit, self.relations, &row)?
        else {
            self.incomplete_rows += 1;
            return Ok(());
        };
        Ok(self.intake.push(relation, row.source.as_ref(), row.target.as_ref(), weight)?)
    }
}

/// The relation and weight of an edge row, or `None` if a field the graph
/// needs is NULL.
fn relation_and_weight<E>(
    graph: &GraphDefinition,
    implicit: Option<RelationIndex>,
    relations: &mut Relations,
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

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use super::*;
    use crate::build::source::tests::{MemorySource, OwnedEdgeRow};
    use crate::build::version::tests::MemoryCatalog;
    use crate::catalog::GraphDefinition;
    use crate::catalog::VersionId;
    use crate::segment::{read_segment, MemorySegmentStore};
    use pgrx::pg_sys;
    use warren_core::graph::{
        AnyCsr, EdgeIntake, GraphCsr, GraphError, IdMap, RelationDictionary, Tombstones,
    };

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
            weighted: false,
            has_relations: false,
            build_reverse: true,
            store_merged: true,
            retain_versions: 2,
            node_order: NodeOrder::Id,
            current_version: None,
            stray_current_version: None,
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
        memory_budget: Option<u64>,
    }

    impl World {
        fn refresh(
            &mut self,
            graph: &GraphDefinition,
            source: &MemorySource,
            promote: bool,
        ) -> VersionId {
            let request = RefreshRequest {
                promote,
                label: Some("l"),
                chunk_size: 16,
                memory_budget: self.memory_budget.unwrap_or(u64::MAX),
            };
            let graph =
                GraphDefinition { current_version: self.catalog.current_version, ..graph.clone() };
            self.try_refresh(&graph, &request, source).unwrap()
        }

        fn try_refresh(
            &mut self,
            graph: &GraphDefinition,
            request: &RefreshRequest<'_>,
            source: &MemorySource,
        ) -> Result<VersionId, BuildError> {
            build(graph, request, &mut source.clone(), &mut self.store, &mut self.catalog)
                .map(|built| built.version)
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
            ..MemorySource::default()
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
        let source = MemorySource { nodes: vec![1], ..MemorySource::default() };
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
            ..MemorySource::default()
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
        let first = MemorySource {
            nodes: vec![10, 20, 30],
            edges: vec![edge(10, 20), edge(20, 30)],
            ..MemorySource::default()
        };
        let v1 = world.refresh(&graph(), &first, true);
        let second = MemorySource {
            nodes: vec![5, 10, 30, 40],
            edges: vec![edge(10, 20), edge(10, 30), edge(40, 5)],
            ..MemorySource::default()
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

    /// DESIGN.md §14 item 22: the first build of a graph in locality order
    /// gives neighbours nearby dense ids; later builds keep them and append
    /// new nodes.
    #[test]
    fn a_first_build_in_locality_order_renumbers_and_later_builds_keep_the_ids() {
        let mut world = World::default();
        let graph = GraphDefinition { node_order: NodeOrder::Locality, ..graph() };
        // The path 10 - 40 - 20 - 50 - 30, whose ids are scrambled along it.
        let path = [(10, 40), (40, 20), (20, 50), (50, 30)];
        let source = MemorySource {
            nodes: vec![10, 20, 30, 40, 50],
            edges: path.iter().map(|&(s, t)| edge(s, t)).collect(),
            ..MemorySource::default()
        };
        let v1 = world.refresh(&graph, &source, true);
        let id_map = world.id_map(v1);
        let dense = |external| id_map.dense_id(&external).unwrap() as i64;
        for (source, target) in path {
            assert_eq!((dense(source) - dense(target)).abs(), 1, "{source} - {target}");
        }
        assert_ne!(dense(10), 0, "not external-id order");
        let forward = world.csr(v1, "", "0");
        assert_eq!(neighbours(&forward, &id_map, 40), [20]);
        assert_eq!(world.catalog.version(v1).stats.edge_count, 4);

        let next = MemorySource {
            nodes: vec![5, 10, 20, 30, 40, 50],
            edges: vec![edge(5, 10), edge(10, 40)],
            ..MemorySource::default()
        };
        let v2 = world.refresh(&graph, &next, true);
        let after = world.id_map(v2);
        for external in [10, 20, 30, 40, 50] {
            assert_eq!(after.dense_id(&external), id_map.dense_id(&external));
        }
        assert_eq!(after.dense_id(&5), Some(5), "appended at the high-water mark");
    }

    #[test]
    fn a_build_starts_from_the_current_version_not_a_staged_one() {
        let mut world = World::default();
        let v1 = world.refresh(
            &graph(),
            &MemorySource { nodes: vec![10], ..MemorySource::default() },
            true,
        );
        world.refresh(
            &graph(),
            &MemorySource { nodes: vec![30], ..MemorySource::default() },
            false,
        );
        let v3 = world.refresh(
            &graph(),
            &MemorySource { nodes: vec![10, 20], ..MemorySource::default() },
            true,
        );
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
            ..MemorySource::default()
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
    fn a_graph_may_leave_its_merged_csr_unstored() {
        let mut world = World::default();
        let stored = GraphDefinition { weighted: true, has_relations: true, ..graph() };
        let unmerged = GraphDefinition { store_merged: false, ..stored.clone() };
        let source = MemorySource {
            nodes: vec![1, 2, 3],
            edges: vec![
                typed_edge(1, 2, "a", 1.0),
                typed_edge(1, 2, "b", 2.0),
                typed_edge(2, 3, "b", 1.0),
            ],
            ..MemorySource::default()
        };
        let v1 = world.refresh(&stored, &source, false);
        let v2 = world.refresh(&unmerged, &source, false);
        assert!(world.has_segment(v1, "rev_offsets:merged"));
        let names = |world: &World, version| -> Vec<String> {
            let names = world.store.chunks.keys().filter(|(v, _, _)| *v == version);
            names.map(|(_, name, _)| name.clone()).collect()
        };
        assert!(names(&world, v2).iter().all(|name| !name.ends_with(":merged")));
        assert!(names(&world, v2).iter().all(|name| names(&world, v1).contains(name)));
        let (r1, r2) = (world.catalog.version(v1), world.catalog.version(v2));
        assert_eq!(r1.stats, r2.stats);
        assert!(r2.bytes < r1.bytes, "{} < {}", r2.bytes, r1.bytes);
    }

    #[test]
    fn a_later_build_keeps_the_relation_indexes() {
        let mut world = World::default();
        let graph = GraphDefinition { has_relations: true, ..graph() };
        let row = |relation: &str| typed_edge(1, 2, relation, 0.0);
        world.refresh(
            &graph,
            &MemorySource {
                nodes: vec![1, 2],
                edges: vec![row("b"), row("a")],
                ..MemorySource::default()
            },
            true,
        );
        let v2 = world.refresh(
            &graph,
            &MemorySource {
                nodes: vec![1, 2],
                edges: vec![row("c"), row("a")],
                ..MemorySource::default()
            },
            true,
        );
        assert_eq!(world.relations(v2).names(), ["b", "a", "c"]);
        assert_eq!(world.csr(v2, "", "0").edge_count(), 0);
    }

    #[test]
    fn undirected_edges_are_stored_in_both_directions_without_reverse_segments() {
        let mut world = World::default();
        let graph = GraphDefinition { directed: false, build_reverse: false, ..graph() };
        let source = MemorySource {
            nodes: vec![1, 2],
            edges: vec![edge(1, 2), edge(2, 2)],
            ..MemorySource::default()
        };
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
        let version = world.refresh(
            &graph,
            &MemorySource { nodes: vec![1], ..MemorySource::default() },
            true,
        );
        assert!(world.relations(version).is_empty());
        assert_eq!(world.csr(version, "", "merged").id_space(), 1);
    }

    #[test]
    fn a_build_over_the_memory_budget_is_refused_before_anything_is_written() {
        let mut world = World::default();
        let source = MemorySource { nodes: (0..10).collect(), ..MemorySource::default() };
        let request =
            RefreshRequest { promote: true, label: None, chunk_size: 16, memory_budget: 64 };
        let refused = world.try_refresh(&graph(), &request, &source);
        assert!(matches!(
            refused,
            Err(BuildError::Graph(GraphError::BuildMemoryLimit { limit: 64, .. }))
        ));
        assert!(world.catalog.versions.is_empty());
        assert!(world.store.chunks.is_empty());
    }

    #[test]
    fn a_build_whose_offsets_exceed_the_budget_is_refused() {
        let mut world = World::default();
        let graph = GraphDefinition { has_relations: true, ..graph() };
        let edges = (0..100).map(|r| typed_edge(r % 10, (r + 1) % 10, &r.to_string(), 0.0));
        let source = MemorySource {
            nodes: (0..10).collect(),
            edges: edges.collect(),
            ..MemorySource::default()
        };
        let request =
            RefreshRequest { promote: true, label: None, chunk_size: 16, memory_budget: 10_000 };
        let refused = world.try_refresh(&graph, &request, &source);
        // Refused at the 56th relation, while reading: 57 CSRs (with the
        // merged one) × 2 directions × 11 offsets × 8 bytes, not after all 100.
        assert!(
            matches!(
                refused,
                Err(BuildError::Graph(GraphError::OffsetsOverBudget {
                    required: 10_032,
                    budget: 10_000
                }))
            ),
            "{refused:?}"
        );
        assert!(world.catalog.versions.is_empty());
    }

    /// The plan's estimate is an upper bound on what the build charges, so
    /// every budget too small for a build is refused before it writes; the
    /// locality order's scratch is charged before the plan's check, so the
    /// same holds in that order.
    #[test]
    fn every_build_refused_for_memory_is_refused_before_it_writes() {
        let edges = (0..300).map(|e| {
            typed_edge(e % 40, (e * 7) % 40, ["a", "b", "c"][e as usize % 3], 0.1 * e as f64)
        });
        let source = MemorySource {
            nodes: (0..40).collect(),
            edges: edges.collect(),
            ..MemorySource::default()
        };
        for node_order in [NodeOrder::Id, NodeOrder::Locality] {
            let graph =
                GraphDefinition { weighted: true, has_relations: true, node_order, ..graph() };
            let mut built = None;
            for budget in (0..40_000).step_by(250) {
                let mut world = World::default();
                let request = RefreshRequest {
                    promote: true,
                    label: None,
                    chunk_size: 16,
                    memory_budget: budget,
                };
                match world.try_refresh(&graph, &request, &source) {
                    Ok(version) => {
                        built = Some((budget, world.catalog.version(version).stats));
                        break;
                    }
                    Err(BuildError::Graph(GraphError::BuildMemoryLimit { .. })) => {
                        assert!(world.catalog.versions.is_empty(), "budget {budget}");
                        assert!(world.store.chunks.is_empty(), "budget {budget}");
                    }
                    Err(error) => panic!("budget {budget}: {error}"),
                }
            }
            let (budget, stats) = built.expect("a budget of 40 kB builds 300 edges");
            assert!(budget > 0);
            assert_eq!(stats.edge_count + stats.deduplicated_edges, 300, "{node_order}");
        }
    }

    /// Edge rows each naming a new relation, `name-<i>`, on `nodes` nodes.
    fn one_relation_per_row(nodes: i64, rows: i64, name: &str) -> MemorySource {
        let edges =
            (0..rows).map(|r| typed_edge(r % nodes, (r + 1) % nodes, &format!("{name}-{r}"), 0.0));
        MemorySource {
            nodes: (0..nodes).collect(),
            edges: edges.collect(),
            ..MemorySource::default()
        }
    }

    /// The offsets of R relations over N nodes are refused as soon as the
    /// relations the edge rows name take them over the budget, not after
    /// the whole edge query has been read.
    #[test]
    fn relations_whose_offsets_exceed_the_budget_are_refused_while_reading() {
        let mut world = World::default();
        let graph = GraphDefinition { has_relations: true, ..graph() };
        let source = one_relation_per_row(10, 30_000, "relation");
        let request =
            RefreshRequest { promote: true, label: None, chunk_size: 16, memory_budget: 1 << 20 };
        let refused = world.try_refresh(&graph, &request, &source);
        assert!(
            matches!(refused, Err(BuildError::Graph(GraphError::OffsetsOverBudget { .. }))),
            "{refused:?}"
        );
        assert!(source.rows_read.get() < 10_000, "{} rows read", source.rows_read.get());
        assert!(world.catalog.versions.is_empty());
    }

    /// The relation dictionary is charged as it grows: with one node, its
    /// names outgrow the offsets, and the build is refused for memory while
    /// reading (DESIGN.md §5.4).
    #[test]
    fn a_relation_dictionary_past_the_budget_is_refused_while_reading() {
        let mut world = World::default();
        let graph = GraphDefinition { has_relations: true, ..graph() };
        let name = "a-relation-name-long-enough-to-outgrow-its-offsets";
        let source = one_relation_per_row(1, 30_000, name);
        let request =
            RefreshRequest { promote: true, label: None, chunk_size: 16, memory_budget: 1 << 20 };
        let refused = world.try_refresh(&graph, &request, &source);
        assert!(
            matches!(
                refused,
                Err(BuildError::Graph(GraphError::BuildMemoryLimit { limit: 1_048_576, .. }))
            ),
            "{refused:?}"
        );
        assert!(source.rows_read.get() < 20_000, "{} rows read", source.rows_read.get());
        assert!(world.catalog.versions.is_empty());
    }

    #[test]
    fn a_wildly_wrong_row_estimate_does_not_fail_the_build() {
        let mut world = World::default();
        for estimate in [0, u64::MAX] {
            let source = MemorySource {
                nodes: vec![1, 2],
                edges: vec![edge(1, 2)],
                estimated_rows: Some(estimate),
                ..MemorySource::default()
            };
            let version = world.refresh(&graph(), &source, true);
            assert_eq!(world.catalog.version(version).stats.edge_count, 1);
        }
    }
}
