//! Compressed sparse row (CSR) adjacency of one relation: `offsets`,
//! `targets` sorted within each row, and optional `weights`
//! (DESIGN.md §5.3).
//!
//! Targets are `u32` when the id space fits, else `u64`. Kernels that want
//! slices are generic over [`Target`] and take a [`Csr<T>`]; everything else
//! uses [`AnyCsr`], which hides the width behind iterators of [`DenseId`].

mod any;
mod build;
mod graph_csr;
mod merge;
mod merged;
mod target;
mod transpose;

use std::ops::Range;

pub use any::{AnyCsr, Neighbours};
pub use graph_csr::{CsrLayout, GraphCsr, RelationCsr};
pub use merged::{AllRelations, MergedNeighbours, MergedRows};
pub use target::{Target, TargetWidth};

use super::memory::held;
use super::{DenseId, GraphError};

/// The CSR of one relation in one direction, with targets of width `T`.
#[derive(Debug, Clone, PartialEq)]
pub struct Csr<T> {
    id_space: u64,
    offsets: Vec<u64>,
    targets: Vec<T>,
    weights: Option<Vec<f64>>,
}

impl<T: Target> Csr<T> {
    /// A CSR from its segments, after checking every invariant: `id_space +
    /// 1` offsets starting at 0, monotonic, ending at the number of targets;
    /// every target inside the id space; a `u32` CSR only below a 2³² id space; each row strictly ascending (sorted,
    /// no parallel edges); one weight per target.
    pub fn from_parts(
        id_space: u64,
        offsets: Vec<u64>,
        targets: Vec<T>,
        weights: Option<Vec<f64>>,
    ) -> Result<Self, GraphError> {
        let rows = id_space.checked_add(1).ok_or(GraphError::IdSpaceTooLarge)?;
        if T::TARGET_WIDTH == TargetWidth::U32
            && TargetWidth::for_id_space(id_space) == TargetWidth::U64
        {
            return Err(GraphError::IdSpaceExceedsTargetWidth);
        }
        if offsets.len() as u64 != rows {
            return Err(GraphError::OffsetsLength { expected: rows, found: offsets.len() as u64 });
        }
        if offsets.first() != Some(&0) {
            return Err(GraphError::FirstOffsetNotZero);
        }
        let edge_count = targets.len() as u64;
        for (row, pair) in (0..).zip(offsets.windows(2)) {
            if pair[1] < pair[0] {
                return Err(GraphError::OffsetsNotMonotone { row });
            }
        }
        if offsets.last() != Some(&edge_count) {
            return Err(GraphError::FinalOffset {
                expected: edge_count,
                found: offsets.last().copied().unwrap_or_default(),
            });
        }
        for (row, pair) in (0..).zip(offsets.windows(2)) {
            let neighbours = &targets[pair[0] as usize..pair[1] as usize];
            if let Some(&target) = neighbours.iter().find(|t| t.dense() >= id_space) {
                return Err(GraphError::TargetOutsideIdSpace { row, target: target.dense() });
            }
            if neighbours.windows(2).any(|w| w[0] >= w[1]) {
                return Err(GraphError::RowNotSorted { row });
            }
        }
        if let Some(weights) = &weights {
            if weights.len() != targets.len() {
                return Err(GraphError::WeightsLength {
                    expected: edge_count,
                    found: weights.len() as u64,
                });
            }
        }
        Ok(Self { id_space, offsets, targets, weights })
    }

    /// The number of rows: the id space of the graph version.
    pub fn id_space(&self) -> u64 {
        self.id_space
    }

    /// The number of edges.
    pub fn edge_count(&self) -> u64 {
        self.targets.len() as u64
    }

    /// Whether the CSR carries weights.
    pub fn is_weighted(&self) -> bool {
        self.weights.is_some()
    }

    /// The `offsets` segment: `id_space + 1` positions into the targets.
    pub fn offsets(&self) -> &[u64] {
        &self.offsets
    }

    /// The `targets` segment: every row's targets, concatenated.
    pub fn targets(&self) -> &[T] {
        &self.targets
    }

    /// The `weights` segment, aligned with the targets.
    pub fn weights(&self) -> Option<&[f64]> {
        self.weights.as_deref()
    }

    /// The heap bytes its offsets, targets and weights hold.
    pub fn bytes(&self) -> u64 {
        held(&self.offsets) + held(&self.targets) + self.weights.as_ref().map_or(0, held)
    }

    /// The targets of `source`'s row in ascending order; empty outside the
    /// id space.
    pub fn neighbours(&self, source: DenseId) -> &[T] {
        &self.targets[self.row(source)]
    }

    /// The weights of `source`'s row, aligned with [`Self::neighbours`].
    pub fn neighbour_weights(&self, source: DenseId) -> Option<&[f64]> {
        let row = self.row(source);
        self.weights.as_deref().map(|weights| &weights[row])
    }

    /// The number of targets in `source`'s row.
    pub fn degree(&self, source: DenseId) -> u64 {
        self.row(source).len() as u64
    }

    /// Whether the edge `source → target` exists (binary search in the row).
    pub fn has_edge(&self, source: DenseId, target: DenseId) -> bool {
        T::from_dense(target)
            .is_some_and(|target| self.neighbours(source).binary_search(&target).is_ok())
    }

    fn row(&self, source: DenseId) -> Range<usize> {
        if source >= self.id_space {
            return 0..0;
        }
        let source = source as usize;
        self.offsets[source] as usize..self.offsets[source + 1] as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 0 → {1, 2}, 1 → {}, 2 → {0, 2}
    fn example() -> Csr<u32> {
        Csr::from_parts(3, vec![0, 2, 2, 4], vec![1, 2, 0, 2], Some(vec![0.5, 1.5, 2.5, 3.5]))
            .unwrap()
    }

    #[test]
    fn the_read_api_answers_from_the_rows() {
        let csr = example();
        assert_eq!(csr.id_space(), 3);
        assert_eq!(csr.edge_count(), 4);
        assert_eq!(csr.neighbours(0), &[1, 2]);
        assert_eq!(csr.neighbours(1), &[] as &[u32]);
        assert_eq!(csr.neighbour_weights(2), Some(&[2.5, 3.5][..]));
        assert_eq!(csr.degree(2), 2);
        assert!(csr.has_edge(2, 2));
        assert!(!csr.has_edge(2, 1));
        assert!(csr.is_weighted());
    }

    #[test]
    fn rows_outside_the_id_space_are_empty() {
        let csr = example();
        assert!(csr.neighbours(3).is_empty());
        assert_eq!(csr.degree(u64::MAX), 0);
        assert!(!csr.has_edge(9, 0));
        assert!(!csr.has_edge(0, u64::MAX));
    }

    #[test]
    fn any_csr_hides_the_target_width() {
        let narrow = AnyCsr::U32(example());
        let wide =
            AnyCsr::U64(Csr::from_parts(3, vec![0, 2, 2, 4], vec![1u64, 2, 0, 2], None).unwrap());
        for csr in [&narrow, &wide] {
            assert_eq!(csr.id_space(), 3);
            assert_eq!(csr.edge_count(), 4);
            assert_eq!(csr.neighbours(0).collect::<Vec<DenseId>>(), [1, 2]);
            assert_eq!(csr.neighbours(0).len(), 2);
            assert_eq!(csr.degree(2), 2);
            assert!(csr.has_edge(0, 2));
            assert!(!csr.has_edge(1, 0));
        }
        assert_eq!(narrow.neighbour_weights(0), Some(&[0.5, 1.5][..]));
        assert_eq!(wide.neighbour_weights(0), None);
    }

    #[test]
    fn a_csr_holds_the_bytes_of_its_offsets_targets_and_weights() {
        assert_eq!(example().bytes(), 4 * 8 + 4 * 4 + 4 * 8);
        let wide = Csr::from_parts(3, vec![0, 2, 2, 4], vec![1u64, 2, 0, 2], None).unwrap();
        assert_eq!(AnyCsr::U64(wide).bytes(), 4 * 8 + 4 * 8);
        assert_eq!(AnyCsr::U32(example()).bytes(), example().bytes());
    }

    fn invalid(offsets: Vec<u64>, targets: Vec<u32>, weights: Option<Vec<f64>>) -> GraphError {
        Csr::from_parts(3, offsets, targets, weights).unwrap_err()
    }

    #[test]
    fn construction_validates_the_csr_invariants() {
        assert_eq!(
            invalid(vec![0, 1, 1], vec![0], None),
            GraphError::OffsetsLength { expected: 4, found: 3 }
        );
        assert_eq!(invalid(vec![1, 1, 1, 1], vec![], None), GraphError::FirstOffsetNotZero);
        assert_eq!(
            invalid(vec![0, 2, 1, 2], vec![0, 1], None),
            GraphError::OffsetsNotMonotone { row: 1 }
        );
        assert_eq!(
            invalid(vec![0, 1, 1, 1], vec![0, 1], None),
            GraphError::FinalOffset { expected: 2, found: 1 }
        );
        assert_eq!(
            invalid(vec![0, 1, 1, 1], vec![3], None),
            GraphError::TargetOutsideIdSpace { row: 0, target: 3 }
        );
        assert_eq!(
            invalid(vec![0, 2, 2, 2], vec![1, 1], None),
            GraphError::RowNotSorted { row: 0 }
        );
        assert_eq!(
            invalid(vec![0, 1, 1, 1], vec![1], Some(vec![])),
            GraphError::WeightsLength { expected: 1, found: 0 }
        );
        assert_eq!(
            Csr::<u64>::from_parts(u64::MAX, vec![0], vec![], None).unwrap_err(),
            GraphError::IdSpaceTooLarge
        );
        assert_eq!(
            Csr::<u32>::from_parts(1 << 32, vec![0], vec![], None).unwrap_err(),
            GraphError::IdSpaceExceedsTargetWidth
        );
    }
}
