//! The reverse CSR, built by transposing a forward CSR (DESIGN.md §5.4
//! step 4): count in-degrees, prefix-sum, scatter while scanning sources in
//! order. O(N + M), and every row comes out sorted. As in the forward build,
//! in-degrees are counted into `offsets[target + 2]`, so the offsets serve
//! as the scatter cursors and end up the reverse CSR's: no cursor array.

use super::{Csr, Target};
use crate::graph::{BuildBuffer, BuildCharge, BuildMemory, GraphError};

/// The transpose of `csr`: an edge `t → s` for every edge `s → t`, with the
/// same weight.
pub(super) fn transpose<T: Target>(csr: &Csr<T>) -> Result<Csr<T>, GraphError> {
    transpose_within(csr, &BuildMemory::unlimited()).map(|(reverse, _)| reverse)
}

/// [`transpose`], its buffers charged to `memory`; returns the charge.
pub(super) fn transpose_within<T: Target>(
    csr: &Csr<T>,
    memory: &BuildMemory,
) -> Result<(Csr<T>, BuildCharge), GraphError> {
    let mut offsets = BuildBuffer::filled(memory, csr.offsets().len(), 0u64)?;
    for target in csr.targets() {
        if let Some(count) = offsets.get_mut(target.dense() as usize + 2) {
            *count += 1;
        }
    }
    for row in 1..offsets.len() {
        offsets[row] += offsets[row - 1];
    }
    let edge_count = csr.targets().len();
    let mut targets = BuildBuffer::filled(memory, edge_count, T::default())?;
    let mut weights = match csr.weights() {
        Some(_) => Some(BuildBuffer::filled(memory, edge_count, 0.0)?),
        None => None,
    };
    for source in 0..csr.id_space() {
        let reversed = T::from_dense(source).ok_or(GraphError::IdSpaceExceedsTargetWidth)?;
        let row_weights = csr.neighbour_weights(source);
        for (i, target) in csr.neighbours(source).iter().enumerate() {
            let cursor = &mut offsets[target.dense() as usize + 1];
            let position = *cursor as usize;
            *cursor += 1;
            targets[position] = reversed;
            if let (Some(weights), Some(row_weights)) = (&mut weights, row_weights) {
                weights[position] = row_weights[i];
            }
        }
    }
    let (offsets, mut charge) = offsets.into_parts();
    let (targets, targets_charge) = targets.into_parts();
    charge.absorb(targets_charge);
    let weights = weights.map(|weights| {
        let (weights, weights_charge) = weights.into_parts();
        charge.absorb(weights_charge);
        weights
    });
    Ok((Csr::from_parts(csr.id_space(), offsets, targets, weights)?, charge))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transposing_reverses_every_edge_and_keeps_rows_sorted() {
        // 0 → {1, 2}, 1 → {2}, 2 → {0, 2}
        let csr = Csr::<u32>::from_parts(
            3,
            vec![0, 2, 3, 5],
            vec![1, 2, 2, 0, 2],
            Some(vec![1.0, 2.0, 3.0, 4.0, 5.0]),
        )
        .unwrap();
        let reverse = transpose(&csr).unwrap();
        // 0 ← {2}, 1 ← {0}, 2 ← {0, 1, 2}
        assert_eq!(reverse.offsets(), &[0, 1, 2, 5]);
        assert_eq!(reverse.targets(), &[2, 0, 0, 1, 2]);
        assert_eq!(reverse.weights(), Some(&[4.0, 1.0, 2.0, 3.0, 5.0][..]));
    }
}
