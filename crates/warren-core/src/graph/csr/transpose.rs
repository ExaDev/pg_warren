//! The reverse CSR, built by transposing a forward CSR (DESIGN.md §5.4
//! step 4): count in-degrees, prefix-sum, scatter while scanning sources in
//! order. O(N + M), and every row comes out sorted.

use super::{Csr, Target};
use crate::graph::{memory, GraphError};

/// The transpose of `csr`: an edge `t → s` for every edge `s → t`, with the
/// same weight.
pub(super) fn transpose<T: Target>(csr: &Csr<T>) -> Result<Csr<T>, GraphError> {
    let row_count = csr.offsets().len() - 1;
    let mut offsets = memory::filled(row_count + 1, 0u64)?;
    for target in csr.targets() {
        offsets[target.dense() as usize + 1] += 1;
    }
    for row in 1..=row_count {
        offsets[row] += offsets[row - 1];
    }
    let mut cursor = memory::with_capacity(row_count)?;
    cursor.extend_from_slice(&offsets[..row_count]);
    let edge_count = csr.targets().len();
    let mut targets = memory::filled(edge_count, T::default())?;
    let mut weights = match csr.weights() {
        Some(_) => Some(memory::filled(edge_count, 0.0)?),
        None => None,
    };
    for source in 0..csr.id_space() {
        let reversed = T::from_dense(source).ok_or(GraphError::IdSpaceExceedsTargetWidth)?;
        let row_weights = csr.neighbour_weights(source);
        for (i, target) in csr.neighbours(source).iter().enumerate() {
            let target = target.dense() as usize;
            let position = cursor[target] as usize;
            cursor[target] += 1;
            targets[position] = reversed;
            if let (Some(weights), Some(row_weights)) = (&mut weights, row_weights) {
                weights[position] = row_weights[i];
            }
        }
    }
    Csr::from_parts(csr.id_space(), offsets, targets, weights)
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
