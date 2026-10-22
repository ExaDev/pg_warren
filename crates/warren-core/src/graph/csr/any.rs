//! A CSR of either target width, behind a width-free read API.

use std::iter::FusedIterator;
use std::slice::Iter;

use super::{Csr, Target, TargetWidth};
use crate::graph::DenseId;

/// A CSR whose target width was chosen from its id space.
#[derive(Debug, Clone, PartialEq)]
pub enum AnyCsr {
    /// Targets stored as `u32` (id space below 2³²).
    U32(Csr<u32>),
    /// Targets stored as `u64`.
    U64(Csr<u64>),
}

impl AnyCsr {
    /// The heap bytes its offsets, targets and weights hold.
    pub fn bytes(&self) -> u64 {
        match self {
            Self::U32(csr) => csr.bytes(),
            Self::U64(csr) => csr.bytes(),
        }
    }

    /// The number of rows: the id space of the graph version.
    pub fn id_space(&self) -> u64 {
        match self {
            Self::U32(csr) => csr.id_space(),
            Self::U64(csr) => csr.id_space(),
        }
    }

    /// The width of the targets.
    pub fn width(&self) -> TargetWidth {
        match self {
            Self::U32(_) => TargetWidth::U32,
            Self::U64(_) => TargetWidth::U64,
        }
    }

    /// The number of edges.
    pub fn edge_count(&self) -> u64 {
        match self {
            Self::U32(csr) => csr.edge_count(),
            Self::U64(csr) => csr.edge_count(),
        }
    }

    /// Whether the CSR carries weights.
    pub fn is_weighted(&self) -> bool {
        match self {
            Self::U32(csr) => csr.is_weighted(),
            Self::U64(csr) => csr.is_weighted(),
        }
    }

    /// The targets of `source`'s row in ascending order; empty outside the
    /// id space.
    pub fn neighbours(&self, source: DenseId) -> Neighbours<'_> {
        match self {
            Self::U32(csr) => Neighbours(Row::U32(csr.neighbours(source).iter())),
            Self::U64(csr) => Neighbours(Row::U64(csr.neighbours(source).iter())),
        }
    }

    /// The weights of `source`'s row, aligned with [`Self::neighbours`].
    pub fn neighbour_weights(&self, source: DenseId) -> Option<&[f64]> {
        match self {
            Self::U32(csr) => csr.neighbour_weights(source),
            Self::U64(csr) => csr.neighbour_weights(source),
        }
    }

    /// The number of targets in `source`'s row.
    pub fn degree(&self, source: DenseId) -> u64 {
        match self {
            Self::U32(csr) => csr.degree(source),
            Self::U64(csr) => csr.degree(source),
        }
    }

    /// Whether the edge `source → target` exists (binary search in the row).
    pub fn has_edge(&self, source: DenseId, target: DenseId) -> bool {
        match self {
            Self::U32(csr) => csr.has_edge(source, target),
            Self::U64(csr) => csr.has_edge(source, target),
        }
    }
}

#[cfg(test)]
impl AnyCsr {
    /// The same CSR with every buffer a shared view of a heap segment.
    pub(crate) fn shared_views(&self) -> Self {
        match self {
            Self::U32(csr) => Self::U32(csr.shared_views()),
            Self::U64(csr) => Self::U64(csr.shared_views()),
        }
    }
}

impl From<Csr<u32>> for AnyCsr {
    fn from(csr: Csr<u32>) -> Self {
        Self::U32(csr)
    }
}

impl From<Csr<u64>> for AnyCsr {
    fn from(csr: Csr<u64>) -> Self {
        Self::U64(csr)
    }
}

/// The targets of one row, as dense ids, in ascending order.
#[derive(Debug, Clone)]
pub struct Neighbours<'a>(Row<'a>);

#[derive(Debug, Clone)]
enum Row<'a> {
    U32(Iter<'a, u32>),
    U64(Iter<'a, u64>),
}

impl Iterator for Neighbours<'_> {
    type Item = DenseId;

    fn next(&mut self) -> Option<DenseId> {
        match &mut self.0 {
            Row::U32(row) => row.next().map(|&target| target.dense()),
            Row::U64(row) => row.next().copied(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            Row::U32(row) => row.size_hint(),
            Row::U64(row) => row.size_hint(),
        }
    }

    /// Skips to the `n`th target in O(1), as the slice iterator does (the
    /// default would step through every target before it).
    fn nth(&mut self, n: usize) -> Option<DenseId> {
        match &mut self.0 {
            Row::U32(row) => row.nth(n).map(|&target| target.dense()),
            Row::U64(row) => row.nth(n).copied(),
        }
    }
}

impl ExactSizeIterator for Neighbours<'_> {}

impl FusedIterator for Neighbours<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nth_skips_ahead_in_a_row_of_either_width() {
        let narrow =
            AnyCsr::from(Csr::<u32>::from_parts(3, vec![0, 3, 3, 3], vec![0, 1, 2], None).unwrap());
        let wide =
            AnyCsr::from(Csr::<u64>::from_parts(3, vec![0, 3, 3, 3], vec![0, 1, 2], None).unwrap());
        for csr in [narrow, wide] {
            let mut row = csr.neighbours(0);
            assert_eq!(row.nth(1), Some(1));
            assert_eq!(row.next(), Some(2));
            assert_eq!(csr.neighbours(0).nth(3), None);
            assert_eq!(csr.neighbours(1).nth(2), None);
        }
    }
}
