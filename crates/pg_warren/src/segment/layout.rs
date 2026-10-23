//! The segment layout of a version (DESIGN.md §5.3): `id_map`,
//! `tombstones`, `rel_dict`, and the `offsets`/`targets`/`weights` of each
//! relation's forward and (if built) reverse CSR, plus the merged CSR's
//! unless there is exactly one relation; and a contraction's `membership`
//! (§6.6). `cache` reads the same layout.
//! Segments are encoded a chunk at a time, so writing one holds a chunk,
//! not the segment.

use warren_core::graph::{
    AnyCsr, BuildCharge, BuildMemory, BuiltCsr, ChunkSink, CsrSink, ExternalId, FixedKeys,
    GraphCsr, IdMap, Membership, RelationCsr, RelationDictionary,
};

use super::{
    csr_segment, write_segment, CsrPart, CsrScope, Direction, SegmentError, SegmentStore, ID_MAP,
    MEMBERSHIP, RELATION_DICTIONARY, TOMBSTONES,
};
use crate::catalog::VersionId;

/// Writes every segment of a version in chunks of `chunk_size` bytes, and
/// returns their total size.
pub(crate) fn write_version<E: ExternalId>(
    segments: &mut impl SegmentStore,
    version: VersionId,
    id_map: &IdMap<FixedKeys<E>>,
    relations: &RelationDictionary,
    graph: &GraphCsr,
    chunk_size: usize,
) -> Result<u64, SegmentError> {
    let memory = BuildMemory::unlimited();
    let mut writer = SegmentWriter::new(segments, version, chunk_size, &memory)?;
    writer.write_identity(id_map, relations)?;
    for (index, relation) in graph.relations().iter().enumerate() {
        writer.write_csrs(CsrScope::Relation(index), relation)?;
    }
    if let Some(merged) = graph.merged() {
        writer.write_csrs(CsrScope::Merged, merged)?;
    }
    Ok(writer.bytes())
}

/// Writes the segments of one version, counting their bytes. As a
/// [`CsrSink`] it writes each CSR a build passes it, which is then freed.
pub(crate) struct SegmentWriter<'s, S> {
    segments: &'s mut S,
    version: VersionId,
    chunk_size: usize,
    memory: BuildMemory,
    /// The build memory for the copies the store makes of the chunk it is
    /// given ([`STORED_COPIES`]).
    _stored_chunk: BuildCharge,
    bytes: u64,
}

/// Copies of the chunk being stored that Postgres holds at once while it
/// inserts it: the `bytea` parameter, the heap tuple, and TOAST's
/// compression buffer and slices. Measured (DESIGN.md §5.4): with 8 MiB
/// chunks a refresh's memory past its charge fell by 11 MB when chunks
/// were 1 MiB, against two copies charged.
const STORED_COPIES: u64 = 4;

/// The build memory a [`SegmentWriter`] holds besides the CSRs it is given:
/// the chunk it encodes into and what the store makes of a chunk.
pub(crate) fn writer_memory(chunk_size: usize) -> u64 {
    (1 + STORED_COPIES) * (chunk_size as u64 + 8)
}

impl<'s, S: SegmentStore> SegmentWriter<'s, S> {
    /// A writer of `version`'s segments in chunks of `chunk_size` bytes,
    /// charging `memory` for its chunks.
    pub(crate) fn new(
        segments: &'s mut S,
        version: VersionId,
        chunk_size: usize,
        memory: &BuildMemory,
    ) -> Result<Self, SegmentError> {
        let _stored_chunk = memory.charge(STORED_COPIES * chunk_size as u64)?;
        Ok(Self { segments, version, chunk_size, memory: memory.clone(), _stored_chunk, bytes: 0 })
    }

    /// The bytes written so far.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The `id_map`, `tombstones` and `rel_dict` segments.
    pub(crate) fn write_identity<E: ExternalId>(
        &mut self,
        id_map: &IdMap<FixedKeys<E>>,
        relations: &RelationDictionary,
    ) -> Result<(), SegmentError> {
        let (chunk_size, memory) = (self.chunk_size, self.memory.clone());
        self.chunked(ID_MAP, |each| id_map.encode_chunked(chunk_size, &memory, each))?;
        let tombstones = id_map.tombstones();
        self.chunked(TOMBSTONES, |each| tombstones.encode_chunked(chunk_size, &memory, each))?;
        let _encoded_charge = memory.charge(relations.encoded_bytes())?;
        let encoded = relations.encode()?;
        self.bytes += write_segment(
            self.segments,
            self.version,
            RELATION_DICTIONARY,
            &encoded,
            self.chunk_size,
        )?;
        Ok(())
    }

    /// The `membership` segment of a contraction's version.
    pub(crate) fn write_membership(&mut self, membership: &Membership) -> Result<(), SegmentError> {
        let (chunk_size, memory) = (self.chunk_size, self.memory.clone());
        self.chunked(MEMBERSHIP, |each| membership.encode_chunked(chunk_size, &memory, each))
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
        let (chunk_size, memory) = (self.chunk_size, self.memory.clone());
        let weights = csr.is_weighted().then_some(CsrPart::Weights);
        for part in [CsrPart::Offsets, CsrPart::Targets].into_iter().chain(weights) {
            self.chunked(&csr_segment(direction, part, scope), |each| {
                Ok(csr.encode_part(part, chunk_size, &memory, each)?.unwrap_or(0))
            })?;
        }
        Ok(())
    }

    /// Stores the chunks `encode` produces as segment `name`, numbered from
    /// 0, and counts its bytes.
    fn chunked(
        &mut self,
        name: &str,
        encode: impl FnOnce(&mut ChunkSink<'_, SegmentError>) -> Result<u64, SegmentError>,
    ) -> Result<(), SegmentError> {
        let (segments, version) = (&mut *self.segments, self.version);
        let mut chunks = 0u64;
        let bytes = encode(&mut |chunk| {
            let chunk_no = i32::try_from(chunks)
                .map_err(|_| SegmentError::TooManyChunks { segment: name.to_owned() })?;
            segments.write_chunk(version, name, chunk_no, chunk)?;
            chunks += 1;
            Ok(())
        })?;
        self.bytes += bytes;
        Ok(())
    }
}

impl<S: SegmentStore> CsrSink for SegmentWriter<'_, S> {
    type Error = SegmentError;

    /// Writes the CSR's segments; the CSR is freed when this returns.
    fn csr(&mut self, built: BuiltCsr) -> Result<(), SegmentError> {
        self.write_csr(built.direction, built.scope, &built.csr)
    }
}
