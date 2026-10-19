//! Encoding of a CSR as its `offsets`, `targets` and `weights` segments.

use super::{decode, elements, encode, read_header, Element, SegmentKind};
use crate::graph::{AnyCsr, Csr, GraphError, Target};

/// The encoded segments of one CSR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsrSegments {
    /// The `offsets` segment.
    pub offsets: Vec<u8>,
    /// The `targets` segment, `u32` or `u64` wide.
    pub targets: Vec<u8>,
    /// The `weights` segment, if the CSR is weighted.
    pub weights: Option<Vec<u8>>,
}

impl AnyCsr {
    /// Encodes this CSR as segments.
    pub fn encode(&self) -> Result<CsrSegments, GraphError> {
        match self {
            Self::U32(csr) => encode_csr(csr),
            Self::U64(csr) => encode_csr(csr),
        }
    }

    /// Decodes a CSR over `id_space` from its segments, validating every
    /// CSR invariant (see [`Csr::from_parts`]).
    pub fn decode(
        id_space: u64,
        offsets: &[u8],
        targets: &[u8],
        weights: Option<&[u8]>,
    ) -> Result<Self, GraphError> {
        let offsets = decode::<u64>(SegmentKind::Offsets, offsets)?;
        let weights =
            weights.map(|weights| decode::<f64>(SegmentKind::Weights, weights)).transpose()?;
        let (header, payload) = read_header(targets, SegmentKind::Targets)?;
        if usize::from(header.width) == u32::WIDTH {
            let targets = elements::<u32>(&header, payload)?;
            Csr::from_parts(id_space, offsets, targets, weights).map(Self::U32)
        } else {
            let targets = elements::<u64>(&header, payload)?;
            Csr::from_parts(id_space, offsets, targets, weights).map(Self::U64)
        }
    }
}

fn encode_csr<T: Target + Element>(csr: &Csr<T>) -> Result<CsrSegments, GraphError> {
    Ok(CsrSegments {
        offsets: encode(SegmentKind::Offsets, csr.offsets())?,
        targets: encode(SegmentKind::Targets, csr.targets())?,
        weights: csr.weights().map(|weights| encode(SegmentKind::Weights, weights)).transpose()?,
    })
}
