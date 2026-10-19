//! The segment layout of a version (DESIGN.md §5.3): `id_map`,
//! `tombstones`, `rel_dict`, and the `offsets`/`targets`/`weights` of each
//! relation's forward and (if built) reverse CSR, plus the merged CSR's
//! unless there is exactly one relation. `cache` reads the same layout.

use warren_core::graph::{AnyCsr, ExternalId, GraphCsr, IdMap, RelationCsr, RelationDictionary};

use super::{
    csr_segment, write_segment, CsrPart, CsrScope, Direction, SegmentError, SegmentStore, ID_MAP,
    RELATION_DICTIONARY, TOMBSTONES,
};
use crate::catalog::VersionId;

/// Writes every segment of a version in chunks of `chunk_size` bytes, and
/// returns their total size.
pub(crate) fn write_version<E: ExternalId>(
    segments: &mut impl SegmentStore,
    version: VersionId,
    id_map: &IdMap<E>,
    relations: &RelationDictionary,
    graph: &GraphCsr,
    chunk_size: usize,
) -> Result<u64, SegmentError> {
    let mut writer = SegmentWriter { segments, version, chunk_size, bytes: 0 };
    writer.write(ID_MAP, &id_map.encode()?)?;
    writer.write(TOMBSTONES, &id_map.tombstones().encode()?)?;
    writer.write(RELATION_DICTIONARY, &relations.encode()?)?;
    for (index, relation) in graph.relations().iter().enumerate() {
        writer.write_csrs(CsrScope::Relation(index), relation)?;
    }
    if let Some(merged) = graph.merged() {
        writer.write_csrs(CsrScope::Merged, merged)?;
    }
    Ok(writer.bytes)
}

/// Writes the segments of one version, counting their bytes.
struct SegmentWriter<'s, S> {
    segments: &'s mut S,
    version: VersionId,
    chunk_size: usize,
    bytes: u64,
}

impl<S: SegmentStore> SegmentWriter<'_, S> {
    fn write(&mut self, name: &str, bytes: &[u8]) -> Result<(), SegmentError> {
        self.bytes += write_segment(self.segments, self.version, name, bytes, self.chunk_size)?;
        Ok(())
    }

    /// The forward and reverse CSRs of a relation, or of the merged CSR.
    fn write_csrs(&mut self, scope: CsrScope, csrs: &RelationCsr) -> Result<(), SegmentError> {
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
    ) -> Result<(), SegmentError> {
        let encoded = csr.encode()?;
        self.write(&csr_segment(direction, CsrPart::Offsets, scope), encoded.offsets())?;
        self.write(&csr_segment(direction, CsrPart::Targets, scope), encoded.targets())?;
        if let Some(weights) = encoded.weights() {
            self.write(&csr_segment(direction, CsrPart::Weights, scope), weights)?;
        }
        Ok(())
    }
}
