//! Encoding of a CSR as its `offsets`, `targets` and `weights` segments,
//! and with sparse offsets its `rows` (DESIGN.md §5.3).

use super::{
    decode, elements, encode, encode_chunked, read_header, ChunkSink, Element, SegmentKind,
};
use crate::graph::{AnyCsr, BuildMemory, Csr, GraphError, Target, TargetWidth};

/// One of a CSR's segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsrPart {
    /// `rows`, with sparse offsets.
    Rows,
    /// `offsets`.
    Offsets,
    /// `targets`.
    Targets,
    /// `weights`.
    Weights,
}

/// The encoded segments of one CSR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsrSegments {
    rows: Option<Vec<u8>>,
    offsets: Vec<u8>,
    targets: Vec<u8>,
    weights: Option<Vec<u8>>,
}

impl CsrSegments {
    /// The `rows` segment, if the CSR's offsets are sparse.
    pub fn rows(&self) -> Option<&[u8]> {
        self.rows.as_deref()
    }

    /// The `offsets` segment.
    pub fn offsets(&self) -> &[u8] {
        &self.offsets
    }

    /// The `targets` segment, `u32` or `u64` wide.
    pub fn targets(&self) -> &[u8] {
        &self.targets
    }

    /// The `weights` segment, if the CSR is weighted.
    pub fn weights(&self) -> Option<&[u8]> {
        self.weights.as_deref()
    }
}

impl AnyCsr {
    /// Encodes this CSR as segments.
    pub fn encode(&self) -> Result<CsrSegments, GraphError> {
        match self {
            Self::U32(csr) => encode_csr(csr),
            Self::U64(csr) => encode_csr(csr),
        }
    }

    /// Encodes one of its segments in chunks of `chunk_size` bytes, through
    /// a chunk buffer charged to `memory`, and returns its length, or `None`
    /// (passing nothing) for the weights of an unweighted CSR and the rows
    /// of one with dense offsets.
    pub fn encode_part<X: From<GraphError>>(
        &self,
        part: CsrPart,
        chunk_size: usize,
        memory: &BuildMemory,
        each: &mut ChunkSink<'_, X>,
    ) -> Result<Option<u64>, X> {
        match self {
            Self::U32(csr) => encode_csr_part(csr, part, chunk_size, memory, each),
            Self::U64(csr) => encode_csr_part(csr, part, chunk_size, memory, each),
        }
    }

    /// Decodes a CSR over `id_space` from its segments, `rows` with sparse
    /// offsets, validating every CSR invariant (see [`Csr::from_parts`] and
    /// [`Csr::from_sparse_parts`]). The targets and rows must have the
    /// width the id space implies ([`TargetWidth::for_id_space`]).
    pub fn decode(
        id_space: u64,
        rows: Option<&[u8]>,
        offsets: &[u8],
        targets: &[u8],
        weights: Option<&[u8]>,
    ) -> Result<Self, GraphError> {
        let offsets = decode::<u64>(SegmentKind::Offsets, offsets)?;
        let weights =
            weights.map(|weights| decode::<f64>(SegmentKind::Weights, weights)).transpose()?;
        let rows = rows.map(|rows| read_header(rows, SegmentKind::Rows)).transpose()?;
        let (header, payload) = read_header(targets, SegmentKind::Targets)?;
        match TargetWidth::for_id_space(id_space) {
            TargetWidth::U32 => {
                let targets = elements::<u32>(&header, payload)?;
                let rows = rows.map(|(header, rows)| elements::<u32>(&header, rows)).transpose()?;
                Csr::from_segments(
                    id_space,
                    rows.map(Into::into),
                    offsets.into(),
                    targets.into(),
                    weights.map(Into::into),
                )
                .map(Self::U32)
            }
            TargetWidth::U64 => {
                let targets = elements::<u64>(&header, payload)?;
                let rows = rows.map(|(header, rows)| elements::<u64>(&header, rows)).transpose()?;
                Csr::from_segments(
                    id_space,
                    rows.map(Into::into),
                    offsets.into(),
                    targets.into(),
                    weights.map(Into::into),
                )
                .map(Self::U64)
            }
        }
    }
}

fn encode_csr<T: Target + Element>(csr: &Csr<T>) -> Result<CsrSegments, GraphError> {
    Ok(CsrSegments {
        rows: csr.rows().map(|rows| encode(SegmentKind::Rows, rows)).transpose()?,
        offsets: encode(SegmentKind::Offsets, csr.offsets())?,
        targets: encode(SegmentKind::Targets, csr.targets())?,
        weights: csr.weights().map(|weights| encode(SegmentKind::Weights, weights)).transpose()?,
    })
}

fn encode_csr_part<T: Target + Element, X: From<GraphError>>(
    csr: &Csr<T>,
    part: CsrPart,
    chunk_size: usize,
    memory: &BuildMemory,
    each: &mut ChunkSink<'_, X>,
) -> Result<Option<u64>, X> {
    fn all<E: Element, X: From<GraphError>>(
        kind: SegmentKind,
        elements: &[E],
        chunk_size: usize,
        memory: &BuildMemory,
        each: &mut ChunkSink<'_, X>,
    ) -> Result<Option<u64>, X> {
        let count = elements.len() as u64;
        encode_chunked(kind, count, elements, chunk_size, memory, each).map(Some)
    }
    match part {
        CsrPart::Rows => match csr.rows() {
            Some(rows) => all(SegmentKind::Rows, rows, chunk_size, memory, each),
            None => Ok(None),
        },
        CsrPart::Offsets => all(SegmentKind::Offsets, csr.offsets(), chunk_size, memory, each),
        CsrPart::Targets => all(SegmentKind::Targets, csr.targets(), chunk_size, memory, each),
        CsrPart::Weights => match csr.weights() {
            Some(weights) => all(SegmentKind::Weights, weights, chunk_size, memory, each),
            None => Ok(None),
        },
    }
}
