//! Loading a version: its segments copied into one aligned region by exact
//! size (`plan`, `fill`), and read through shared views of it into the
//! structures `warren-core` computes on, checked to agree (`views`;
//! DESIGN.md §5.3, §5.5, §5.6).

mod fill;
mod plan;
mod views;

pub(super) use fill::fill;
pub(super) use plan::plan;

use std::sync::{Arc, Weak};

use pgrx::PgSqlErrorCode;
use thiserror::Error;
use warren_core::graph::{
    BuildCharge, BuildMemory, ExternalId, GraphCsr, GraphError, IdMap, RelationDictionary,
};
use warren_core::version::{GraphVersion, VersionError};

use super::region::{HeapMemory, Region, RegionError, RegionMemory};
use crate::access::SqlError;
use crate::catalog::VersionId;
use crate::extension::graph_sqlstate;
use crate::segment::{
    read_segment_within, SegmentError, SegmentStore, ID_MAP, RELATION_DICTIONARY, TOMBSTONES,
};
use crate::source::IdDatum;

/// Why a version could not be loaded.
#[derive(Debug, Error)]
pub(crate) enum LoadError {
    /// A segment could not be read.
    #[error(transparent)]
    Segment(#[from] SegmentError),
    /// A segment failed to decode or validate.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// The segments decode but do not agree with each other.
    #[error(transparent)]
    Version(#[from] VersionError),
    /// The region could not be allocated, or does not hold its layout.
    #[error(transparent)]
    Region(#[from] RegionError),
}

impl SqlError for LoadError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::Segment(error) => error.sqlstate(),
            Self::Graph(error) => graph_sqlstate(error),
            Self::Version(_) => PgSqlErrorCode::ERRCODE_DATA_CORRUPTED,
            Self::Region(error) => error.sqlstate(),
        }
    }
}

/// A loaded version.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LoadedVersion<E> {
    /// Its id map, tombstones included.
    pub(crate) id_map: IdMap<E>,
    /// Its relation dictionary.
    pub(crate) relations: RelationDictionary,
    /// Its CSRs. A loaded graph reports no deduplicated edges: that count
    /// is kept in `warren.versions`, not in the segments.
    pub(crate) graph: GraphCsr,
}

impl<E: ExternalId> LoadedVersion<E> {
    /// The version as `warren-core`'s view of it.
    pub(crate) fn version(&self) -> Result<GraphVersion<'_, E>, VersionError> {
        GraphVersion::new(&self.id_map, &self.relations, &self.graph)
    }

    /// The heap bytes it holds: what it counts against
    /// `warren.max_cache_memory`. For a version loaded into a region, the
    /// bytes of its arrays there and of its decoded relation dictionary,
    /// the same as when each array was a buffer of its own; the region's
    /// 64-byte header, 32 bytes of table of contents and under 64 bytes of
    /// alignment padding per array, and its encoded relation dictionary,
    /// are not counted.
    pub(crate) fn bytes(&self) -> u64 {
        self.id_map.bytes() + self.relations.bytes() + self.graph.bytes()
    }
}

/// The id map and relation dictionary of a version, each with the build
/// memory it holds.
pub(crate) struct Identity<E> {
    /// The id map.
    pub(crate) id_map: IdMap<E>,
    /// The charge for the id map.
    pub(crate) id_map_charge: BuildCharge,
    /// The relation dictionary.
    pub(crate) relations: RelationDictionary,
    /// The charge for the relation dictionary.
    pub(crate) relations_charge: BuildCharge,
}

/// The id map and relation dictionary of `version`, without its CSRs: what
/// a build needs of the version it starts from. Their segments, their
/// decoding and what they hold are charged to `memory`.
pub(crate) fn load_identity<E: ExternalId>(
    segments: &mut impl SegmentStore,
    version: VersionId,
    memory: &BuildMemory,
) -> Result<Identity<E>, LoadError> {
    let (tombstones, _tombstones_charge) =
        read_segment_within(segments, version, TOMBSTONES, memory)?;
    let (id_map, _id_map_charge) = read_segment_within(segments, version, ID_MAP, memory)?;
    let (id_map, id_map_charge) = IdMap::decode_within(&id_map, &tombstones, memory)?;
    let (encoded, _encoded_charge) =
        read_segment_within(segments, version, RELATION_DICTIONARY, memory)?;
    // Decoded first, then charged: it holds about twice the encoding,
    // whose bytes are charged while it decodes.
    let relations = RelationDictionary::decode(&encoded)?;
    let relations_charge = memory.charge(relations.bytes())?;
    Ok(Identity { id_map, id_map_charge, relations, relations_charge })
}

/// Loads every segment of `version` into one region, allocated by its
/// exact size (an allocation failure is an error, 53200, not an abort),
/// and validates each array as decoding does and then that they agree:
/// the CSRs cover the id map's id space with the target width it implies,
/// there is one CSR per relation, a merged CSR without relations, or with
/// several if the graph stores it (`store_merged`), and reverse CSRs for
/// all or none.
pub(crate) fn load<E: IdDatum>(
    segments: &mut impl SegmentStore,
    version: VersionId,
    store_merged: bool,
) -> Result<LoadedVersion<E>, LoadError> {
    let plan = plan::<E>(segments, version, store_merged)?;
    let mut memory = HeapMemory::zeroed(plan.len())?;
    fill::<E>(segments, version, &plan, memory.bytes_mut())?;
    Ok(read(Box::new(memory), version)?.0)
}

/// The version `memory` holds, filled by [`fill`], read through views of
/// its region and validated as [`load`] validates it, and the region,
/// which lives while any view of it does.
pub(super) fn read<E: IdDatum>(
    memory: Box<dyn RegionMemory>,
    version: VersionId,
) -> Result<(LoadedVersion<E>, Weak<Region>), LoadError> {
    let region = Region::open(memory, version)?;
    Ok((views::loaded(&region)?, Arc::downgrade(&region)))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use super::*;
    use crate::segment::{write_version, MemorySegmentStore};
    use crate::source::IdDatum;
    use warren_core::graph::{
        AnyCsr, CsrLayout, EdgeIntake, GraphCsr, IdMap, RelationCsr, RelationDictionary,
    };

    /// A version built in memory: ids 10, 20, 30, 40 (or as given) with
    /// the second tombstoned.
    struct Built<E = i64> {
        id_map: IdMap<E>,
        relations: RelationDictionary,
        graph: GraphCsr,
    }

    fn built(relations: &[&str], weighted: bool, reverse: bool) -> Built {
        built_as(relations, weighted, CsrLayout { build_reverse: reverse, store_merged: true })
    }

    fn built_as(relations: &[&str], weighted: bool, layout: CsrLayout) -> Built {
        built_with([10i64, 20, 30, 40], relations, weighted, layout)
    }

    fn built_with<E: IdDatum>(
        ids: [E; 4],
        relations: &[&str],
        weighted: bool,
        layout: CsrLayout,
    ) -> Built<E> {
        let id_map = IdMap::new()
            .rebuild(ids.to_vec())
            .and_then(|m| m.rebuild(vec![ids[0], ids[2], ids[3]]))
            .unwrap();
        let mut dictionary = RelationDictionary::new();
        let mut intake = EdgeIntake::directed(&id_map, weighted);
        for (i, name) in relations.iter().enumerate() {
            let relation = dictionary.intern(name).unwrap();
            intake.push(relation, Some(&ids[0]), Some(&ids[2]), i as f64 + 0.5).unwrap();
            let target = if i % 2 == 0 { ids[0] } else { ids[3] };
            intake.push(relation, Some(&ids[3]), Some(&target), 1.0).unwrap();
        }
        let (edges, _) = intake.finish();
        let graph = GraphCsr::build_as(edges, dictionary.len(), id_map.id_space(), layout).unwrap();
        Built { id_map, relations: dictionary, graph }
    }

    fn stored<E: IdDatum>(version: &Built<E>) -> MemorySegmentStore {
        stored_in_chunks(version, 32)
    }

    fn stored_in_chunks<E: IdDatum>(version: &Built<E>, chunk_size: usize) -> MemorySegmentStore {
        let mut store = MemorySegmentStore::default();
        let (id_map, relations, graph) = (&version.id_map, &version.relations, &version.graph);
        write_version(&mut store, 7, id_map, relations, graph, chunk_size).unwrap();
        store
    }

    fn round_trip(relations: &[&str], weighted: bool, reverse: bool) {
        loads_as_written(&built(relations, weighted, reverse));
    }

    fn loads_as_written(version: &Built) {
        let store_merged = version.graph.merged().is_some() || version.relations.len() < 2;
        let loaded = load::<i64>(&mut stored(version), 7, store_merged).unwrap();
        assert_eq!(loaded.id_map, version.id_map);
        assert_eq!(loaded.relations, version.relations);
        assert_eq!(loaded.graph, version.graph);
        assert!(loaded.version().is_ok());
    }

    #[test]
    fn a_version_with_one_relation_loads_as_written() {
        round_trip(&[""], false, true);
    }

    #[test]
    fn a_version_with_weighted_relations_and_a_merged_csr_loads_as_written() {
        round_trip(&["cites", "extends", "funds"], true, true);
    }

    #[test]
    fn a_version_without_its_merged_csr_loads_as_written() {
        for build_reverse in [true, false] {
            let layout = CsrLayout { build_reverse, store_merged: false };
            let version = built_as(&["cites", "extends", "funds"], true, layout);
            loads_as_written(&version);
            // What the cache holds, and so counts against its cap, is the
            // stored version's bytes less its merged CSR's.
            let loaded = load::<i64>(&mut stored(&version), 7, false).unwrap();
            let with_merged = built(&["cites", "extends", "funds"], true, build_reverse);
            let with_merged = load::<i64>(&mut stored(&with_merged), 7, true).unwrap();
            let merged = with_merged.graph.merged().map_or(0, |merged| merged.bytes());
            assert!(merged > 0);
            assert_eq!(loaded.bytes() + merged, with_merged.bytes());
        }
    }

    #[test]
    fn a_version_without_reverse_csrs_loads_as_written() {
        round_trip(&["a", "b"], true, false);
    }

    #[test]
    fn a_version_with_relations_but_no_edges_loads_as_written() {
        round_trip(&[], false, true);
    }

    #[test]
    fn the_identity_of_a_version_loads_without_its_csrs() {
        let version = built(&["a", "b"], false, true);
        let mut store = stored(&version);
        store.chunks.retain(|(_, name, _), _| !name.contains(':'));
        let memory = BuildMemory::unlimited();
        let Identity { id_map, id_map_charge, relations, relations_charge } =
            load_identity::<i64>(&mut store, 7, &memory).unwrap();
        assert_eq!(id_map, version.id_map);
        assert_eq!(relations, version.relations);
        assert_eq!(id_map_charge.bytes(), id_map.bytes());
        assert_eq!(relations_charge.bytes(), relations.bytes());
        assert_eq!(memory.in_use(), id_map.bytes() + relations.bytes());
        // The segments and the decoding's scratch were charged while it loaded.
        assert!(memory.peak() > 2 * id_map.bytes(), "{}", memory.peak());
        let tight = BuildMemory::new(id_map.bytes());
        assert!(matches!(
            load_identity::<i64>(&mut store, 7, &tight),
            Err(LoadError::Graph(GraphError::BuildMemoryLimit { .. }))
                | Err(LoadError::Segment(SegmentError::Graph(GraphError::BuildMemoryLimit { .. })))
        ));
    }

    #[test]
    fn a_missing_segment_is_an_error() {
        let version = built(&["a"], false, true);
        let mut store = stored(&version);
        store.chunks.retain(|(_, name, _), _| name != "targets:0");
        assert!(matches!(
            load::<i64>(&mut store, 7, true),
            Err(LoadError::Segment(SegmentError::SegmentMissing { version: 7, .. }))
        ));
    }

    #[test]
    fn a_lost_merged_csr_is_an_error_where_the_graph_stores_it() {
        let mut store = stored(&built(&["a", "b"], false, true));
        store.chunks.retain(|(_, name, _), _| !name.ends_with(":merged"));
        assert!(matches!(
            load::<i64>(&mut store, 7, true),
            Err(LoadError::Segment(SegmentError::SegmentMissing { version: 7, .. }))
        ));
        assert!(load::<i64>(&mut store, 7, false).is_ok());
    }

    #[test]
    fn segments_that_disagree_are_an_error() {
        let mut store = stored(&built(&["a"], false, true));
        let other = built(&["a", "b"], false, true);
        let mut other_store = stored(&other);
        let rel_dict = other_store.chunks.remove(&(7, "rel_dict".to_owned(), 0)).unwrap();
        store.chunks.insert((7, "rel_dict".to_owned(), 0), rel_dict);
        assert!(load::<i64>(&mut store, 7, true).is_err());
    }

    #[test]
    fn ids_of_every_type_load_as_written_whatever_the_chunk_size() {
        fn check<E: IdDatum>(ids: [E; 4]) {
            let layout = CsrLayout { build_reverse: true, store_merged: true };
            let version = built_with(ids, &["a", "b"], true, layout);
            for chunk_size in [1, 5, 7, 12, 32, 4096] {
                let mut store = stored_in_chunks(&version, chunk_size);
                let loaded = load::<E>(&mut store, 7, true).unwrap();
                assert_eq!(loaded.id_map, version.id_map, "chunks of {chunk_size}");
                assert_eq!(loaded.graph, version.graph);
            }
        }
        check([-7i32, 10, 30, i32::MAX]);
        check([i64::MIN, 10, 30, 40]);
        check([[0u8; 16], [1; 16], [2; 16], [255; 16]]);
    }

    #[test]
    fn every_array_of_a_loaded_version_is_aligned_in_one_region() {
        let layout = CsrLayout { build_reverse: true, store_merged: true };
        let version = built_with([10i32, 20, 30, 40], &["a", "b", "c"], true, layout);
        let loaded = load::<i32>(&mut stored(&version), 7, true).unwrap();
        let mut starts = Vec::new();
        let mut csrs: Vec<&RelationCsr> = loaded.graph.relations().iter().collect();
        csrs.extend(loaded.graph.merged());
        for relation in csrs {
            for csr in [relation.forward()].into_iter().chain(relation.reverse()) {
                let AnyCsr::U32(csr) = csr else { panic!("u64 targets") };
                starts.push(csr.offsets().as_ptr() as usize);
                starts.push(csr.targets().as_ptr() as usize);
                starts.extend(csr.weights().map(|weights| weights.as_ptr() as usize));
            }
        }
        assert_eq!(starts.len(), 4 * 2 * 3);
        assert!(starts.iter().all(|start| start % 64 == 0), "{starts:x?}");
        // One region: each array follows the last within an alignment.
        for pair in starts.windows(2) {
            assert!(pair[1] > pair[0] && pair[1] - pair[0] < 1024, "{starts:x?}");
        }
    }

    #[test]
    fn a_segment_whose_header_disagrees_with_its_length_is_an_error() {
        let mut store = stored(&built(&["a"], false, true));
        let first = store.chunks.get_mut(&(7, "offsets:0".to_owned(), 0)).unwrap();
        first[8] += 1;
        assert!(matches!(
            load::<i64>(&mut store, 7, true),
            Err(LoadError::Segment(SegmentError::Graph(GraphError::SegmentLength { .. })))
                | Err(LoadError::Graph(GraphError::SegmentLength { .. }))
        ));
    }

    #[test]
    fn a_segment_of_another_kind_is_an_error() {
        let mut store = stored(&built(&["a"], true, true));
        let targets = store.chunks.get(&(7, "targets:0".to_owned(), 0)).unwrap().clone();
        store.chunks.insert((7, "offsets:0".to_owned(), 0), targets);
        assert!(matches!(
            load::<i64>(&mut store, 7, true),
            Err(LoadError::Segment(SegmentError::Graph(GraphError::SegmentKind { .. })))
                | Err(LoadError::Graph(GraphError::SegmentKind { .. }))
        ));
    }

    #[test]
    fn a_dense_id_outside_the_id_space_is_an_error() {
        let version = built(&["a"], false, true);
        let mut store = stored_in_chunks(&version, 4096);
        let id_map = store.chunks.get_mut(&(7, "id_map".to_owned(), 0)).unwrap();
        // The first pair's dense id.
        id_map[16 + 8..16 + 16].copy_from_slice(&99u64.to_le_bytes());
        assert!(matches!(
            load::<i64>(&mut store, 7, true),
            Err(LoadError::Graph(GraphError::IdMapNotPermutation))
        ));
    }

    #[test]
    fn tombstones_of_another_id_space_are_an_error() {
        let version = built(&["a"], false, true);
        let mut store = stored_in_chunks(&version, 4096);
        let tombstones = store.chunks.get_mut(&(7, "tombstones".to_owned(), 0)).unwrap();
        tombstones[8..16].copy_from_slice(&3u64.to_le_bytes());
        assert!(matches!(
            load::<i64>(&mut store, 7, true),
            Err(LoadError::Segment(SegmentError::Graph(GraphError::TombstonesLength {
                expected: 4,
                found: 3
            }))) | Err(LoadError::Graph(GraphError::TombstonesLength { expected: 4, found: 3 }))
        ));
    }
}
