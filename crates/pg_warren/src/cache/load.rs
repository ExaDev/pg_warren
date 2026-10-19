//! Loading a version: its segments decoded and checked to agree, into the
//! structures `warren-core` computes on (DESIGN.md §5.3, §5.5).

use pgrx::PgSqlErrorCode;
use thiserror::Error;
use warren_core::graph::{
    AnyCsr, ExternalId, GraphCsr, GraphError, IdMap, RelationCsr, RelationDictionary, Tombstones,
};
use warren_core::version::{GraphVersion, VersionError};

use crate::access::SqlError;
use crate::catalog::VersionId;
use crate::extension::graph_sqlstate;
use crate::segment::{
    csr_segment, read_optional_segment, read_segment, CsrPart, CsrScope, Direction, SegmentError,
    SegmentStore, ID_MAP, RELATION_DICTIONARY, TOMBSTONES,
};

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
}

impl SqlError for LoadError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::Segment(error) => error.sqlstate(),
            Self::Graph(error) => graph_sqlstate(error),
            Self::Version(_) => PgSqlErrorCode::ERRCODE_DATA_CORRUPTED,
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
}

/// The id map and relation dictionary of `version`, without its CSRs: what
/// a build needs of the version it starts from.
pub(crate) fn load_identity<E: ExternalId>(
    segments: &mut impl SegmentStore,
    version: VersionId,
) -> Result<(IdMap<E>, RelationDictionary), LoadError> {
    let tombstones = Tombstones::decode(&read_segment(segments, version, TOMBSTONES)?)?;
    let id_map = IdMap::decode(&read_segment(segments, version, ID_MAP)?, tombstones)?;
    let relations =
        RelationDictionary::decode(&read_segment(segments, version, RELATION_DICTIONARY)?)?;
    Ok((id_map, relations))
}

/// Loads every segment of `version`, validating each as it is decoded and
/// then that they agree: the CSRs cover the id map's id space with the
/// target width it implies, there is one CSR per relation, a merged CSR
/// unless there is exactly one relation, and reverse CSRs for all or none.
pub(crate) fn load<E: ExternalId>(
    segments: &mut impl SegmentStore,
    version: VersionId,
) -> Result<LoadedVersion<E>, LoadError> {
    let (id_map, relations) = load_identity(segments, version)?;
    let id_space = id_map.id_space();
    let mut csrs = Vec::new();
    for index in 0..relations.len() {
        csrs.push(relation_csr(segments, version, id_space, CsrScope::Relation(index))?);
    }
    let merged = match relations.len() {
        1 => None,
        _ => Some(relation_csr(segments, version, id_space, CsrScope::Merged)?),
    };
    let graph = GraphCsr::from_parts(id_space, csrs, merged, 0)?;
    let loaded = LoadedVersion { id_map, relations, graph };
    loaded.version()?;
    Ok(loaded)
}

/// The forward CSR of `scope`, and its reverse if one was stored.
fn relation_csr(
    segments: &mut impl SegmentStore,
    version: VersionId,
    id_space: u64,
    scope: CsrScope,
) -> Result<RelationCsr, LoadError> {
    let forward = csr(segments, version, id_space, Direction::Forward, scope)?;
    let reverse_offsets = csr_segment(Direction::Reverse, CsrPart::Offsets, scope);
    let reverse = match read_optional_segment(segments, version, &reverse_offsets)? {
        Some(_) => Some(csr(segments, version, id_space, Direction::Reverse, scope)?),
        None => None,
    };
    Ok(RelationCsr::new(forward, reverse)?)
}

fn csr(
    segments: &mut impl SegmentStore,
    version: VersionId,
    id_space: u64,
    direction: Direction,
    scope: CsrScope,
) -> Result<AnyCsr, LoadError> {
    let name = |part| csr_segment(direction, part, scope);
    let offsets = read_segment(segments, version, &name(CsrPart::Offsets))?;
    let targets = read_segment(segments, version, &name(CsrPart::Targets))?;
    let weights = read_optional_segment(segments, version, &name(CsrPart::Weights))?;
    Ok(AnyCsr::decode(id_space, &offsets, &targets, weights.as_deref())?)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use super::*;
    use crate::segment::{write_version, MemorySegmentStore};
    use warren_core::graph::{EdgeIntake, GraphCsr, IdMap, RelationDictionary};

    /// A version built in memory: ids 10, 20, 30, 40 with 20 tombstoned.
    struct Built {
        id_map: IdMap<i64>,
        relations: RelationDictionary,
        graph: GraphCsr,
    }

    fn built(relations: &[&str], weighted: bool, reverse: bool) -> Built {
        let id_map = IdMap::new()
            .rebuild(vec![10, 20, 30, 40])
            .and_then(|m| m.rebuild(vec![10, 30, 40]))
            .unwrap();
        let mut dictionary = RelationDictionary::new();
        let mut intake = EdgeIntake::directed(&id_map, weighted);
        for (i, name) in relations.iter().enumerate() {
            let relation = dictionary.intern(name).unwrap();
            intake.push(relation, Some(&10), Some(&30), i as f64 + 0.5).unwrap();
            intake.push(relation, Some(&40), Some(&(10 + 30 * (i as i64 % 2))), 1.0).unwrap();
        }
        let (edges, _) = intake.finish();
        let graph = GraphCsr::build(edges, dictionary.len(), id_map.id_space(), reverse).unwrap();
        Built { id_map, relations: dictionary, graph }
    }

    fn stored(version: &Built) -> MemorySegmentStore {
        let mut store = MemorySegmentStore::default();
        write_version(&mut store, 7, &version.id_map, &version.relations, &version.graph, 32)
            .unwrap();
        store
    }

    fn round_trip(relations: &[&str], weighted: bool, reverse: bool) {
        let version = built(relations, weighted, reverse);
        let loaded = load::<i64>(&mut stored(&version), 7).unwrap();
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
        let (id_map, relations) = load_identity::<i64>(&mut store, 7).unwrap();
        assert_eq!(id_map, version.id_map);
        assert_eq!(relations, version.relations);
    }

    #[test]
    fn a_missing_segment_is_an_error() {
        let version = built(&["a"], false, true);
        let mut store = stored(&version);
        store.chunks.retain(|(_, name, _), _| name != "targets:0");
        assert!(matches!(
            load::<i64>(&mut store, 7),
            Err(LoadError::Segment(SegmentError::SegmentMissing { version: 7, .. }))
        ));
    }

    #[test]
    fn segments_that_disagree_are_an_error() {
        let mut store = stored(&built(&["a"], false, true));
        let other = built(&["a", "b"], false, true);
        let mut other_store = stored(&other);
        let rel_dict = other_store.chunks.remove(&(7, "rel_dict".to_owned(), 0)).unwrap();
        store.chunks.insert((7, "rel_dict".to_owned(), 0), rel_dict);
        assert!(load::<i64>(&mut store, 7).is_err());
    }
}
