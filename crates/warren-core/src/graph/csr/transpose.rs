//! The reverse CSR, built by transposing a forward CSR (DESIGN.md §5.4
//! step 4): count in-degrees, lay out the rows, scatter while scanning
//! sources in order. O(N + M) for dense offsets, and O(M + R log R) for R
//! stored rows with sparse ones; every row comes out sorted. Dense
//! offsets count in-degrees into `offsets[target + 2]`, so the offsets
//! serve as the scatter cursors and end up the reverse CSR's; a relation's
//! reverse CSR is placed through the build's [`RowCursors`] instead, so its
//! offsets can be sparse.

use super::cursors::{RowCursors, RowIndex};
use super::{Csr, Target};
use crate::graph::{BuildBuffer, BuildCharge, BuildMemory, GraphError};

/// The transpose of `csr` with dense offsets: an edge `t → s` for every
/// edge `s → t`, with the same weight.
pub(super) fn transpose<T: Target>(csr: &Csr<T>) -> Result<Csr<T>, GraphError> {
    transpose_within(csr, &BuildMemory::unlimited()).map(|(reverse, _)| reverse)
}

/// [`transpose`], its buffers charged to `memory`; returns the charge.
pub(super) fn transpose_within<T: Target>(
    csr: &Csr<T>,
    memory: &BuildMemory,
) -> Result<(Csr<T>, BuildCharge), GraphError> {
    let rows = usize::try_from(csr.id_space()).map_err(|_| GraphError::IdSpaceTooLarge)?;
    let mut offsets = BuildBuffer::filled(memory, rows + 1, 0u64)?;
    for target in csr.targets() {
        if let Some(count) = offsets.get_mut(target.dense() as usize + 2) {
            *count += 1;
        }
    }
    for row in 1..offsets.len() {
        offsets[row] += offsets[row - 1];
    }
    let mut index = RowIndex { rows: None, offsets };
    let place = |index: &mut RowIndex<T>, target: T| {
        let cursor = &mut index.offsets[target.dense() as usize + 1];
        let position = *cursor;
        *cursor += 1;
        position as usize
    };
    let (targets, weights) = scattered(csr, &mut index, memory, place)?;
    assembled(csr.id_space(), index, targets, weights)
}

/// The transpose of a relation's `csr`, its rows placed through
/// `cursors`, which are left zeroed: with sparse offsets where they pay
/// ([`super::sparse::stores_sparse`]). Its buffers are charged to
/// `memory`; returns the charge.
pub(super) fn transpose_with<T: Target>(
    csr: &Csr<T>,
    cursors: &mut RowCursors,
    memory: &BuildMemory,
) -> Result<(Csr<T>, BuildCharge), GraphError> {
    let mut index = cursors.index::<T, _>(true, || csr.targets().iter().map(|t| t.dense()))?;
    let place = |_: &mut RowIndex<T>, target: T| cursors.place(target.dense());
    let scattered = scattered(csr, &mut index, memory, place);
    cursors.clear(index.rows.as_deref(), &index.offsets);
    let (targets, weights) = scattered?;
    assembled(csr.id_space(), index, targets, weights)
}

/// The reverse CSR's targets and weights: scanning `csr`'s rows in order,
/// each entry `s → t` goes to the position `place` gives for row `t`.
fn scattered<T: Target>(
    csr: &Csr<T>,
    index: &mut RowIndex<T>,
    memory: &BuildMemory,
    mut place: impl FnMut(&mut RowIndex<T>, T) -> usize,
) -> Result<(BuildBuffer<T>, Option<BuildBuffer<f64>>), GraphError> {
    let edge_count = csr.targets().len();
    let mut targets = BuildBuffer::filled(memory, edge_count, T::default())?;
    let mut weights = match csr.weights() {
        Some(_) => Some(BuildBuffer::filled(memory, edge_count, 0.0)?),
        None => None,
    };
    let slices = csr.slices();
    for (source, positions) in slices.stored_rows() {
        let reversed = T::from_dense(source).ok_or(GraphError::IdSpaceExceedsTargetWidth)?;
        let row_weights = slices.weights_at(positions.clone());
        for (i, &target) in slices.targets_at(positions).iter().enumerate() {
            let position = place(index, target);
            targets[position] = reversed;
            if let (Some(weights), Some(row_weights)) = (&mut weights, row_weights) {
                weights[position] = row_weights[i];
            }
        }
    }
    Ok((targets, weights))
}

/// The CSR of a transposition's parts, and the charge for its buffers.
fn assembled<T: Target>(
    id_space: u64,
    index: RowIndex<T>,
    targets: BuildBuffer<T>,
    weights: Option<BuildBuffer<f64>>,
) -> Result<(Csr<T>, BuildCharge), GraphError> {
    let (offsets, mut charge) = index.offsets.into_parts();
    let rows = index.rows.map(|rows| {
        let (rows, rows_charge) = rows.into_parts();
        charge.absorb(rows_charge);
        rows
    });
    let (targets, targets_charge) = targets.into_parts();
    charge.absorb(targets_charge);
    let weights = weights.map(|weights| {
        let (weights, weights_charge) = weights.into_parts();
        charge.absorb(weights_charge);
        weights
    });
    let csr = match rows {
        Some(rows) => Csr::from_sparse_parts(id_space, rows, offsets, targets, weights),
        None => Csr::from_parts(id_space, offsets, targets, weights),
    };
    Ok((csr?, charge))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 0 → {1, 2}, 1 → {2}, 2 → {0, 2}
    fn forward() -> Csr<u32> {
        let weights = Some(vec![1.0, 2.0, 3.0, 4.0, 5.0]);
        Csr::from_parts(3, vec![0, 2, 3, 5], vec![1, 2, 2, 0, 2], weights).unwrap()
    }

    #[test]
    fn transposing_reverses_every_edge_and_keeps_rows_sorted() {
        let reverse = transpose(&forward()).unwrap();
        // 0 ← {2}, 1 ← {0}, 2 ← {0, 1, 2}
        assert_eq!(reverse.offsets(), &[0, 1, 2, 5]);
        assert_eq!(reverse.targets(), &[2, 0, 0, 1, 2]);
        assert_eq!(reverse.weights(), Some(&[4.0, 1.0, 2.0, 3.0, 5.0][..]));
    }

    #[test]
    fn a_relation_transposes_to_sparse_offsets_where_they_pay() {
        // 900 → {3, 7}, 950 → {7} over 1,000 nodes, stored sparsely.
        let weights = Some(vec![1.0, 2.0, 3.0]);
        let sparse = Csr::<u32>::from_sparse_parts(
            1000,
            vec![900, 950],
            vec![0, 2, 3],
            vec![3, 7, 7],
            weights,
        )
        .unwrap();
        let memory = BuildMemory::unlimited();
        let mut cursors = RowCursors::new(&memory, 1000).unwrap();
        let (reverse, _) = transpose_with(&sparse, &mut cursors, &memory).unwrap();
        // 3 ← {900}, 7 ← {900, 950}
        assert_eq!(reverse.rows(), Some(&[3, 7][..]));
        assert_eq!(reverse.offsets(), &[0, 1, 3]);
        assert_eq!(reverse.targets(), &[900, 900, 950]);
        assert_eq!(reverse.weights(), Some(&[1.0, 2.0, 3.0][..]));
        assert_eq!(transpose(&sparse).unwrap(), reverse.densified());
        // Every row of a small id space: dense offsets, as `transpose`'s.
        let mut cursors = RowCursors::new(&memory, 3).unwrap();
        let (dense, _) = transpose_with(&forward(), &mut cursors, &memory).unwrap();
        assert_eq!(dense, transpose(&forward()).unwrap());
    }
}
