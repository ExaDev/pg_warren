//! The contraction pipeline (DESIGN.md §6.6): a parent version and a label
//! vector in, a new version of the quotient graph out. Its node ids are
//! the distinct labels, its dense ids continue from its current version's
//! as a refresh's do (§5.2), its edges are `warren_core`'s contraction of
//! the parent's CSRs, and the version is written through the build's own
//! tail, with the membership as one more segment. Pure logic over the
//! build's ports (segments, catalog).

use warren_core::community::{contract, Contraction, Labels};
use warren_core::compute::{AnyVector, Workers};
use warren_core::graph::{
    AnyCsr, BuildCharge, BuildMemory, CsrLayout, ExternalId, GraphCsr, IdMap, RelationDictionary,
    RelationIndex,
};
use warren_core::interrupts::Interrupts;

use super::CommunitySqlError;
use crate::build::{write_contents, Catalog, Contents, Derivation, RefreshRequest};
use crate::cache::{load_identity, Identity, LoadedVersion};
use crate::catalog::{GraphDefinition, Lineage, VersionId};
use crate::segment::SegmentStore;

/// What a contraction is asked for, beyond the graphs.
pub(crate) struct ContractRequest<'a> {
    /// How the new version is written and whether it is promoted.
    pub(crate) write: RefreshRequest<'a>,
    /// How edges are aggregated.
    pub(crate) contraction: Contraction,
    /// The parent version and the label vector, as recorded.
    pub(crate) lineage: Lineage,
}

/// Contracts `parent` by `labels` into a new version of `quotient` (a
/// graph derived by contraction, its row locked), and returns its id.
pub(crate) fn contract_version<E: ExternalId>(
    quotient: &GraphDefinition,
    parent: &LoadedVersion<E>,
    labels: &AnyVector,
    request: &ContractRequest<'_>,
    pool: (&Workers, &mut impl Interrupts),
    segments: &mut impl SegmentStore,
    catalog: &mut impl Catalog,
) -> Result<VersionId, CommunitySqlError> {
    let memory = BuildMemory::new(request.write.memory_budget);
    let labels = Labels::read(labels, parent.id_map.tombstones(), &memory)
        .map_err(CommunitySqlError::Labels)?;
    let Identity { id_map: base, id_map_charge, relations, mut relations_charge } =
        match quotient.current_version {
            Some(version) => load_identity(segments, version, &memory)?,
            None => Identity {
                id_map: IdMap::new(),
                id_map_charge: BuildCharge::none(&memory),
                relations: RelationDictionary::new(),
                relations_charge: BuildCharge::none(&memory),
            },
        };
    let (id_map, _id_map_charge) = base.rebuild_within(labels.distinct(&memory)?)?;
    drop((base, id_map_charge));
    let (membership, _membership_charge) = labels.membership(&id_map, &memory)?;
    drop(labels);
    let mut relations = relations;
    let mut pairs: Vec<(RelationIndex, &AnyCsr)> = Vec::new();
    for (name, csr) in parent.relations.names().iter().zip(parent.graph.relations()) {
        let index = relations.intern_within(name, &mut relations_charge)?;
        pairs.push((index, csr.forward()));
    }
    let layout =
        CsrLayout { build_reverse: quotient.build_reverse, store_merged: quotient.store_merged };
    let budget = request.write.memory_budget;
    GraphCsr::check_offsets(relations.len(), id_map.id_space(), layout, budget)?;
    let (workers, interrupts) = pool;
    let edges = contract(workers, interrupts, &pairs, &membership, request.contraction, &memory)?;
    let contents = Contents { id_map, relations, edges, dropped_edges: 0 };
    let derivation = Derivation { lineage: request.lineage, membership: &membership };
    Ok(write_contents(
        quotient,
        &request.write,
        contents,
        Some(derivation),
        &memory,
        segments,
        catalog,
    )?)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::pg_sys;
    use warren_core::compute::{ComputeMemory, Vector};
    use warren_core::graph::{EdgeIntake, Membership, NodeOrder, Tombstones};
    use warren_core::interrupts::Uninterrupted;

    use super::*;
    use crate::build::MemoryCatalog;
    use crate::catalog::GraphSource;
    use crate::segment::{read_segment, MemorySegmentStore, MEMBERSHIP};

    fn quotient() -> GraphDefinition {
        GraphDefinition {
            graph_id: 2,
            name: "q".to_owned(),
            source: GraphSource::Contraction,
            owner: pg_sys::Oid::INVALID,
            readers: Vec::new(),
            id_type: crate::source::IdType::Int8,
            node_query: String::new(),
            edge_query: String::new(),
            search_path: String::new(),
            directed: true,
            weighted: true,
            has_relations: true,
            build_reverse: true,
            store_merged: true,
            retain_versions: 2,
            node_order: NodeOrder::Id,
            current_version: None,
            stray_current_version: None,
        }
    }

    /// Nodes 10, 20, 30, 40 (30 tombstoned); edges 10 → 20 (`a`, 1.0),
    /// 20 → 40 (`b`, 2.0) and 40 → 10 (`b`, 4.0).
    fn parent() -> LoadedVersion<i64> {
        let id_map = IdMap::new()
            .rebuild(vec![10, 20, 30, 40])
            .and_then(|m| m.rebuild(vec![10, 20, 40]))
            .unwrap();
        let mut relations = RelationDictionary::new();
        let (a, b) = (relations.intern("a").unwrap(), relations.intern("b").unwrap());
        let mut intake = EdgeIntake::directed(&id_map, true);
        intake.push(a, Some(&10), Some(&20), 1.0).unwrap();
        intake.push(b, Some(&20), Some(&40), 2.0).unwrap();
        intake.push(b, Some(&40), Some(&10), 4.0).unwrap();
        let graph = GraphCsr::build(intake.finish().0, 2, 4, true).unwrap();
        LoadedVersion { id_map, relations, graph }
    }

    fn labels(values: [f64; 4]) -> AnyVector {
        let memory = ComputeMemory::new(u64::MAX);
        AnyVector::F64(Vector::from_values(&memory, 4, 1, values.to_vec()).unwrap())
    }

    fn request(parent_version: VersionId) -> ContractRequest<'static> {
        ContractRequest {
            write: RefreshRequest {
                promote: true,
                label: None,
                chunk_size: 16,
                memory_budget: u64::MAX,
            },
            contraction: Contraction { weighted: true, drop_self_loops: false, undirected: false },
            lineage: Lineage { parent_version, label_vector: Some(77) },
        }
    }

    struct World {
        store: MemorySegmentStore,
        catalog: MemoryCatalog,
        workers: Workers,
    }

    impl World {
        fn new() -> Self {
            Self {
                store: MemorySegmentStore::default(),
                catalog: MemoryCatalog::default(),
                workers: Workers::new(1).unwrap(),
            }
        }

        fn contract(&mut self, labels: &AnyVector) -> Result<VersionId, CommunitySqlError> {
            let quotient =
                GraphDefinition { current_version: self.catalog.current_version, ..quotient() };
            contract_version(
                &quotient,
                &parent(),
                labels,
                &request(5),
                (&self.workers, &mut Uninterrupted),
                &mut self.store,
                &mut self.catalog,
            )
        }

        fn id_map(&mut self, version: VersionId) -> IdMap<i64> {
            let tombstones =
                Tombstones::decode(&read_segment(&mut self.store, version, "tombstones").unwrap())
                    .unwrap();
            IdMap::decode(&read_segment(&mut self.store, version, "id_map").unwrap(), tombstones)
                .unwrap()
        }

        fn loaded(&mut self, version: VersionId) -> LoadedVersion<i64> {
            crate::cache::load(&mut self.store, version, true).unwrap()
        }
    }

    #[test]
    fn a_contraction_records_a_version_with_its_lineage_and_membership() {
        let mut world = World::new();
        // 10 and 40 in community 7, 20 in community -3; 30 is tombstoned.
        let version = world.contract(&labels([7.0, -3.0, f64::NAN, 7.0])).unwrap();
        let row = world.catalog.version(version).clone();
        assert_eq!(row.lineage, Some(Lineage { parent_version: 5, label_vector: Some(77) }));
        assert_eq!((row.stats.node_count, row.stats.id_space, row.stats.edge_count), (2, 2, 3));
        assert_eq!(world.catalog.current_version, Some(version));
        let id_map = world.id_map(version);
        assert_eq!((id_map.dense_id(&-3), id_map.dense_id(&7)), (Some(0), Some(1)));
        let membership =
            Membership::decode(&read_segment(&mut world.store, version, MEMBERSHIP).unwrap(), 2)
                .unwrap();
        assert_eq!(
            [0, 1, 2, 3].map(|n| membership.community(n)),
            [Some(1), Some(0), None, Some(1)]
        );
        let loaded = world.loaded(version);
        assert_eq!(loaded.relations.names(), ["a", "b"]);
        let a = loaded.graph.relations()[0].forward();
        assert_eq!(
            (a.neighbours(1).collect::<Vec<_>>(), a.neighbour_weights(1)),
            (vec![0], Some(&[1.0][..]))
        );
        let b = loaded.graph.relations()[1].forward();
        assert_eq!(b.neighbours(0).collect::<Vec<_>>(), [1]);
        assert_eq!(b.neighbour_weights(1), Some(&[4.0][..]), "40 → 10 inside community 7");
        assert_eq!(row.bytes, world.store.chunks.values().map(|c| c.len() as u64).sum::<u64>());
    }

    #[test]
    fn labels_that_persist_keep_their_dense_ids() {
        let mut world = World::new();
        let first = world.contract(&labels([7.0, -3.0, 0.0, 7.0])).unwrap();
        let second = world.contract(&labels([7.0, 9.0, 0.0, 9.0])).unwrap();
        let (before, after) = (world.id_map(first), world.id_map(second));
        assert_eq!(after.dense_id(&7), before.dense_id(&7));
        assert_eq!(after.dense_id(&-3), None);
        assert!(after.tombstones().contains(before.dense_id(&-3).unwrap()));
        assert_eq!(after.dense_id(&9), Some(2));
    }

    #[test]
    fn a_label_that_is_not_an_integer_is_refused_before_anything_is_written() {
        let mut world = World::new();
        let refused = world.contract(&labels([7.0, 0.5, 0.0, 7.0]));
        assert!(matches!(refused, Err(CommunitySqlError::Labels(_))), "{refused:?}");
        assert!(world.catalog.versions.is_empty());
        assert!(world.store.chunks.is_empty());
    }
}
