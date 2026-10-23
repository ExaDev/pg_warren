//! A loaded version read from its region: every array a shared view of
//! it, validated once as the decoded segments are (DESIGN.md §5.3, §5.6).

use std::sync::Arc;

use warren_core::graph::{AnyCsr, Csr, GraphCsr, RelationCsr, RelationDictionary};

use super::{CachedIds, LoadError, LoadedVersion};
use crate::cache::region::{Array, Region, RegionError};
use crate::segment::{CsrPart, CsrScope, Direction};

/// The version `region` holds, checked as `load` checks a version: each
/// array's invariants, then that they agree.
pub(super) fn loaded<E: CachedIds>(region: &Arc<Region>) -> Result<LoadedVersion<E>, LoadError> {
    let missing = RegionError::MissingArray;
    let words = region.view::<u64>(Array::Tombstones)?.ok_or(missing("tombstones"))?;
    let id_map = E::view(region, words)?;
    let id_space = id_map.id_space();
    let encoded = region.array_bytes(Array::RelationDictionary);
    let relations = RelationDictionary::decode(encoded.ok_or(missing("relation dictionary"))?)?;
    let mut csrs = Vec::with_capacity(relations.len());
    for index in 0..relations.len() {
        let csr = relation_csr(region, id_space, CsrScope::Relation(index))?;
        csrs.push(csr.ok_or(missing("relation's CSR"))?);
    }
    let merged = relation_csr(region, id_space, CsrScope::Merged)?;
    let graph = GraphCsr::from_parts(id_space, csrs, merged, 0)?;
    let loaded = LoadedVersion { id_map, relations, graph };
    loaded.version()?;
    Ok(loaded)
}

/// The forward CSR of `scope` and its reverse, if the region holds them.
fn relation_csr(
    region: &Arc<Region>,
    id_space: u64,
    scope: CsrScope,
) -> Result<Option<RelationCsr>, LoadError> {
    let Some(forward) = csr(region, id_space, Direction::Forward, scope)? else {
        return Ok(None);
    };
    let reverse = csr(region, id_space, Direction::Reverse, scope)?;
    Ok(Some(RelationCsr::new(forward, reverse)?))
}

/// The CSR of `scope` in `direction`, if the region holds its offsets; its
/// targets are as wide as their entry says, which `Csr::from_segments`
/// checks against the id space.
fn csr(
    region: &Arc<Region>,
    id_space: u64,
    direction: Direction,
    scope: CsrScope,
) -> Result<Option<AnyCsr>, LoadError> {
    let array = |part| Array::Csr(direction, part, scope);
    let Some(offsets) = region.view::<u64>(array(CsrPart::Offsets))? else {
        return Ok(None);
    };
    let weights = region.view::<f64>(array(CsrPart::Weights))?;
    let targets = array(CsrPart::Targets);
    let width = region.layout().entry(targets).map(|entry| entry.width);
    let missing = RegionError::MissingArray("CSR's targets");
    let csr = match width.ok_or(missing)? {
        4 => {
            let targets = region.view::<u32>(targets)?.ok_or(missing)?;
            AnyCsr::U32(Csr::from_segments(id_space, offsets, targets, weights)?)
        }
        _ => {
            let targets = region.view::<u64>(targets)?.ok_or(missing)?;
            AnyCsr::U64(Csr::from_segments(id_space, offsets, targets, weights)?)
        }
    };
    Ok(Some(csr))
}
