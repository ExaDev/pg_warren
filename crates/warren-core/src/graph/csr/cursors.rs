//! Row cursors: where a build places each entry of a relation's CSR, in
//! one array over the id space shared by every CSR it builds, so that a
//! CSR's own offsets can be sparse (DESIGN.md §5.3, §5.4 step 4). Between
//! CSRs every cursor is 0, and a CSR with sparse offsets touches only its
//! stored rows' cursors, so building it costs its own rows and edges.

use super::sparse::stores_sparse;
use super::Target;
use crate::graph::{BuildBuffer, BuildMemory, DenseId, GraphError};

/// Marks a row already collected into the stored rows while counting (a
/// count never reaches it).
const COLLECTED: u64 = 1 << 63;

/// A cursor per row of the id space, all 0 between CSRs.
#[derive(Debug)]
pub(super) struct RowCursors {
    cursors: BuildBuffer<u64>,
}

/// The offsets of a CSR being built, and its stored rows if they are
/// sparse.
#[derive(Debug)]
pub(super) struct RowIndex<T> {
    pub(super) rows: Option<BuildBuffer<T>>,
    pub(super) offsets: BuildBuffer<u64>,
}

/// Each stored row (every row, with dense offsets) and its start.
fn starts<'a, T: Target>(
    rows: Option<&'a [T]>,
    offsets: &'a [u64],
) -> impl Iterator<Item = (usize, u64)> + 'a {
    let ids = rows.map(|rows| rows.iter().map(|row| row.dense() as usize));
    let all = ids.is_none().then(|| 0..offsets.len().saturating_sub(1));
    ids.into_iter().flatten().chain(all.into_iter().flatten()).zip(offsets.iter().copied())
}

impl RowCursors {
    /// Zeroed cursors for `id_space` rows, charged to `memory`.
    pub(super) fn new(memory: &BuildMemory, id_space: u64) -> Result<Self, GraphError> {
        let rows = usize::try_from(id_space).map_err(|_| GraphError::IdSpaceTooLarge)?;
        Ok(Self { cursors: BuildBuffer::filled(memory, rows, 0)? })
    }

    /// Counts the entries of each row, one source per entry from
    /// `sources` (called once, or twice for sparse offsets), and returns
    /// the CSR's offsets: sparse where [`stores_sparse`] says so and
    /// `sparse` allows it, else dense. Leaves each row's cursor at its
    /// start.
    pub(super) fn index<T: Target, I: Iterator<Item = DenseId>>(
        &mut self,
        sparse: bool,
        sources: impl Fn() -> I,
    ) -> Result<RowIndex<T>, GraphError> {
        let memory = self.cursors.memory().clone();
        let id_space = self.cursors.len() as u64;
        let mut stored = 0usize;
        for source in sources() {
            let cursor = &mut self.cursors[source as usize];
            stored += usize::from(*cursor == 0);
            *cursor += 1;
        }
        if !(sparse && stores_sparse(stored as u64, id_space, T::TARGET_WIDTH)) {
            let mut offsets = BuildBuffer::filled(&memory, self.cursors.len() + 1, 0)?;
            let mut end = 0;
            for (row, cursor) in self.cursors.iter_mut().enumerate() {
                let count = std::mem::replace(cursor, end);
                end += count;
                offsets[row + 1] = end;
            }
            return Ok(RowIndex { rows: None, offsets });
        }
        let mut rows = BuildBuffer::with_capacity(&memory, stored)?;
        for source in sources() {
            let cursor = &mut self.cursors[source as usize];
            if *cursor & COLLECTED == 0 {
                *cursor |= COLLECTED;
                rows.push(T::from_dense(source).ok_or(GraphError::IdSpaceExceedsTargetWidth)?)?;
            }
        }
        rows.sort_unstable();
        let mut offsets = BuildBuffer::with_capacity(&memory, stored + 1)?;
        let mut end = 0;
        offsets.push(end)?;
        for row in rows.iter() {
            let cursor = &mut self.cursors[row.dense() as usize];
            let count = std::mem::replace(cursor, end) & !COLLECTED;
            end += count;
            offsets.push(end)?;
        }
        Ok(RowIndex { rows: Some(rows), offsets })
    }

    /// The position of `source`'s next entry, advancing its cursor.
    #[inline]
    pub(super) fn place(&mut self, source: DenseId) -> usize {
        let cursor = &mut self.cursors[source as usize];
        let position = *cursor;
        *cursor += 1;
        position as usize
    }

    /// Puts the cursor of each row of a CSR's `rows` (every row if
    /// `None`) back at the row's start in `offsets`, to place a second
    /// column.
    pub(super) fn rewind<T: Target>(&mut self, rows: Option<&[T]>, offsets: &[u64]) {
        for (row, start) in starts(rows, offsets) {
            self.cursors[row] = start;
        }
    }

    /// Zeroes the cursors of a CSR's `rows` (every row if `None`), for the
    /// next CSR.
    pub(super) fn clear<T: Target>(&mut self, rows: Option<&[T]>, offsets: &[u64]) {
        for (row, _) in starts(rows, offsets) {
            self.cursors[row] = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Places `sources` in order, returning the index and each entry's
    /// position.
    fn placed(
        cursors: &mut RowCursors,
        sparse: bool,
        sources: &[u64],
    ) -> (RowIndex<u32>, Vec<usize>) {
        let index = cursors.index::<u32, _>(sparse, || sources.iter().copied()).unwrap();
        let positions = sources.iter().map(|&source| cursors.place(source)).collect();
        (index, positions)
    }

    #[test]
    fn a_few_rows_get_sparse_offsets_and_entries_keep_their_order_within_a_row() {
        let memory = BuildMemory::unlimited();
        let mut cursors = RowCursors::new(&memory, 1000).unwrap();
        let (index, positions) = placed(&mut cursors, true, &[700, 3, 700, 3, 3]);
        assert_eq!(index.rows.as_deref(), Some(&[3, 700][..]));
        assert_eq!(&*index.offsets, &[0, 3, 5]);
        assert_eq!(positions, [3, 0, 4, 1, 2]);
        cursors.rewind(index.rows.as_deref(), &index.offsets);
        assert_eq!([cursors.place(3), cursors.place(700)], [0, 3]);
        cursors.clear(index.rows.as_deref(), &index.offsets);
        assert!(cursors.cursors.iter().all(|&cursor| cursor == 0));
    }

    #[test]
    fn many_rows_or_no_sparse_offsets_allowed_get_dense_offsets() {
        let memory = BuildMemory::unlimited();
        let mut cursors = RowCursors::new(&memory, 4).unwrap();
        let (index, positions) = placed(&mut cursors, true, &[2, 0, 2, 1]);
        assert!(index.rows.is_none());
        assert_eq!(&*index.offsets, &[0, 1, 2, 4, 4]);
        assert_eq!(positions, [2, 0, 3, 1]);
        cursors.clear(index.rows.as_deref(), &index.offsets);
        let mut cursors = RowCursors::new(&memory, 1000).unwrap();
        let (index, _) = placed(&mut cursors, false, &[5]);
        assert!(index.rows.is_none());
        assert_eq!(index.offsets.len(), 1001);
        cursors.clear(index.rows.as_deref(), &index.offsets);
        assert!(cursors.cursors.iter().all(|&cursor| cursor == 0));
    }
}
