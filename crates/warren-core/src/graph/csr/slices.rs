//! A CSR's arrays borrowed once, for kernels that read many rows: a row
//! read through them touches no segment buffer, whose every access costs
//! a virtual call when it is a shared view (DESIGN.md §5.6).

use std::ops::Range;

use super::{Csr, Target};
use crate::graph::DenseId;

/// The offsets, targets and weights of one CSR, borrowed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CsrSlices<'a, T> {
    /// The stored rows' ids, with sparse offsets.
    rows: Option<&'a [T]>,
    offsets: &'a [u64],
    targets: &'a [T],
    weights: Option<&'a [f64]>,
}

impl<T: Target> Csr<T> {
    /// Its arrays, borrowed once.
    pub(crate) fn slices(&self) -> CsrSlices<'_, T> {
        CsrSlices {
            rows: self.rows(),
            offsets: self.offsets(),
            targets: self.targets(),
            weights: self.weights(),
        }
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

    /// Each row it stores in ascending order, with the positions of its
    /// entries in the targets: every row with dense offsets, the stored
    /// (non-empty) rows with sparse ones.
    pub(crate) fn stored_rows(&self) -> impl Iterator<Item = (DenseId, Range<usize>)> + 'a {
        let ids = self.rows.map(|rows| rows.iter().map(|row| row.dense()));
        let all = ids.is_none().then(|| 0..self.offsets.len().saturating_sub(1) as DenseId);
        let ids = ids.into_iter().flatten().chain(all.into_iter().flatten());
        ids.zip(self.offsets.windows(2))
            .map(|(row, pair)| (row, pair[0] as usize..pair[1] as usize))
    }

    /// The targets of the entries at `positions`.
    pub(crate) fn targets_at(&self, positions: Range<usize>) -> &'a [T] {
        &self.targets[positions]
    }

    /// The weights of the entries at `positions`, if weighted.
    pub(crate) fn weights_at(&self, positions: Range<usize>) -> Option<&'a [f64]> {
        self.weights.map(|weights| &weights[positions])
    }

    /// [`Self::row`], for sources read mostly in ascending order, as a
    /// product sweeps its rows: with sparse offsets the search starts at
    /// `hint`, where the last seek left it, and gallops forward, so a sweep
    /// costs O(1) per row rather than a binary search; a source below the
    /// last one searches from the start.
    #[inline]
    pub(crate) fn seek(&self, source: DenseId, hint: &mut usize) -> Range<usize> {
        let Some(rows) = self.rows else { return self.row(source) };
        let Some(source) = T::from_dense(source) else { return 0..0 };
        let from = match *hint {
            from if from <= rows.len() && (from == 0 || rows[from - 1] < source) => from,
            _ => 0,
        };
        // rows[..lo] are below `source`.
        let (mut lo, mut step) = (from, 1);
        while lo + step <= rows.len() && rows[lo + step - 1] < source {
            lo += step;
            step *= 2;
        }
        let hi = (lo + step).min(rows.len());
        let position = lo + rows[lo..hi].partition_point(|&row| row < source);
        *hint = position;
        match rows.get(position) == Some(&source) {
            true => self.offsets[position] as usize..self.offsets[position + 1] as usize,
            false => 0..0,
        }
    }

    /// The positions of `source`'s row in the targets; empty outside the
    /// id space, and with sparse offsets for a row not stored (found by
    /// binary search in the stored rows).
    #[inline]
    fn row(&self, source: DenseId) -> Range<usize> {
        let position = match self.rows {
            None => usize::try_from(source).ok(),
            Some(rows) => T::from_dense(source).and_then(|source| rows.binary_search(&source).ok()),
        };
        match position.and_then(|s| self.offsets.get(s..s.checked_add(2)?)) {
            Some(&[start, end]) => start as usize..end as usize,
            _ => 0..0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeking_from_a_hint_finds_the_rows_a_lookup_finds_in_any_order() {
        let rows = vec![3, 10, 11, 50, 99];
        let targets = vec![1, 2, 3, 4, 5, 6];
        let sparse =
            Csr::<u32>::from_sparse_parts(100, rows, vec![0, 1, 2, 4, 5, 6], targets, None)
                .unwrap();
        let dense = sparse.densified();
        for csr in [&sparse, &dense] {
            let slices = csr.slices();
            let mut hint = 0;
            let sources = (0..105).chain([50, 3, 99, 0, 11, 11, 12, 98, u64::MAX, 4]);
            for source in sources {
                assert_eq!(slices.seek(source, &mut hint), slices.row(source), "{source}");
            }
        }
    }
}
