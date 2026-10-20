//! The k-way merge of sorted CSR rows of one target width into one
//! ascending, distinct row, without allocating for up to [`INLINE`] rows.

use super::Target;
use crate::graph::DenseId;

/// Rows held inline; a merge of more spills them to the heap.
const INLINE: usize = 8;

/// The ascending, distinct union of strictly ascending rows: each step
/// takes the smallest head and advances every row it heads.
#[derive(Debug, Clone)]
pub(super) struct RowMerge<'g, T> {
    inline: [&'g [T]; INLINE],
    len: usize,
    spilled: Vec<&'g [T]>,
}

impl<'g, T: Target> RowMerge<'g, T> {
    /// The merge of `rows`; empty rows are skipped.
    pub(super) fn new(rows: impl Iterator<Item = &'g [T]>) -> Self {
        let mut merge = Self { inline: [&[]; INLINE], len: 0, spilled: Vec::new() };
        for row in rows.filter(|row| !row.is_empty()) {
            match (merge.spilled.is_empty(), merge.len < INLINE) {
                (true, true) => {
                    merge.inline[merge.len] = row;
                    merge.len += 1;
                }
                (true, false) => {
                    merge.spilled.extend_from_slice(&merge.inline);
                    merge.spilled.push(row);
                }
                (false, _) => merge.spilled.push(row),
            }
        }
        merge
    }

    fn rows(&mut self) -> &mut [&'g [T]] {
        match self.spilled.is_empty() {
            true => &mut self.inline[..self.len],
            false => &mut self.spilled,
        }
    }

    /// The next target: the smallest head, once.
    pub(super) fn next_target(&mut self) -> Option<DenseId> {
        let rows = self.rows();
        if let [row] = rows {
            let (&head, rest) = row.split_first()?;
            *row = rest;
            return Some(head.dense());
        }
        let smallest = *rows.iter().filter_map(|row| row.first()).min()?;
        for row in rows.iter_mut() {
            if let [head, rest @ ..] = row {
                if *head == smallest {
                    *row = rest;
                }
            }
        }
        Some(smallest.dense())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged(rows: &[&[u32]]) -> Vec<DenseId> {
        let mut merge = RowMerge::new(rows.iter().copied());
        std::iter::from_fn(|| merge.next_target()).collect()
    }

    #[test]
    fn rows_merge_ascending_and_distinct() {
        assert_eq!(merged(&[&[1, 4, 9], &[], &[0, 4], &[9]]), [0, 1, 4, 9]);
        assert_eq!(merged(&[&[2, 3]]), [2, 3]);
        assert_eq!(merged(&[]), [] as [DenseId; 0]);
        assert_eq!(merged(&[&[], &[]]), [] as [DenseId; 0]);
    }

    #[test]
    fn more_rows_than_fit_inline_spill_to_the_heap() {
        let rows: Vec<Vec<u32>> = (0..20).map(|r| vec![r, r + 1, 40]).collect();
        let slices: Vec<&[u32]> = rows.iter().map(Vec::as_slice).collect();
        let expected: Vec<DenseId> = (0..=20).chain([40]).collect();
        assert_eq!(merged(&slices), expected);
    }
}
