//! A CSR's arrays borrowed once, for kernels that read many rows: a row
//! read through them touches no segment buffer, whose every access costs
//! a virtual call when it is a shared view (DESIGN.md §5.6).

use std::ops::Range;

use super::{Csr, Target};
use crate::graph::DenseId;

/// The offsets, targets and weights of one CSR, borrowed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CsrSlices<'a, T> {
    offsets: &'a [u64],
    targets: &'a [T],
    weights: Option<&'a [f64]>,
}

impl<T: Target> Csr<T> {
    /// Its arrays, borrowed once.
    pub(crate) fn slices(&self) -> CsrSlices<'_, T> {
        CsrSlices { offsets: self.offsets(), targets: self.targets(), weights: self.weights() }
    }
}

impl<'a, T: Target> CsrSlices<'a, T> {
    /// The targets of `source`'s row in ascending order; empty outside the
    /// id space.
    pub(crate) fn neighbours(&self, source: DenseId) -> &'a [T] {
        &self.targets[self.row(source)]
    }

    /// The weights of `source`'s row, aligned with [`Self::neighbours`].
    pub(crate) fn neighbour_weights(&self, source: DenseId) -> Option<&'a [f64]> {
        let row = self.row(source);
        self.weights.map(|weights| &weights[row])
    }

    /// The number of targets in `source`'s row.
    pub(crate) fn degree(&self, source: DenseId) -> u64 {
        self.row(source).len() as u64
    }

    /// The positions of `source`'s row in the targets; empty outside the
    /// id space (whose size is one less than the offsets').
    fn row(&self, source: DenseId) -> Range<usize> {
        let source = usize::try_from(source).ok();
        match source.and_then(|s| self.offsets.get(s..s.checked_add(2)?)) {
            Some(&[start, end]) => start as usize..end as usize,
            _ => 0..0,
        }
    }
}
