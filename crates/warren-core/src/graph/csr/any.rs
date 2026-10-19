//! A CSR of either target width, behind a width-free read API.

use std::iter::{Copied, FusedIterator, Map};
use std::slice::Iter;

use super::{Csr, Target};
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
    /// The number of rows: the id space of the graph version.
    pub fn id_space(&self) -> u64 {
        match self {
            Self::U32(csr) => csr.id_space(),
            Self::U64(csr) => csr.id_space(),
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
            Self::U32(csr) => Neighbours::U32(csr.neighbours(source).iter().map(|&t| t.dense())),
            Self::U64(csr) => Neighbours::U64(csr.neighbours(source).iter().copied()),
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

/// The targets of one row, as dense ids.
#[derive(Debug, Clone)]
pub enum Neighbours<'a> {
    /// Over a `u32` row.
    U32(Map<Iter<'a, u32>, fn(&u32) -> DenseId>),
    /// Over a `u64` row.
    U64(Copied<Iter<'a, u64>>),
}

impl Iterator for Neighbours<'_> {
    type Item = DenseId;

    fn next(&mut self) -> Option<DenseId> {
        match self {
            Self::U32(row) => row.next(),
            Self::U64(row) => row.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::U32(row) => row.size_hint(),
            Self::U64(row) => row.size_hint(),
        }
    }
}

impl ExactSizeIterator for Neighbours<'_> {}

impl FusedIterator for Neighbours<'_> {}
