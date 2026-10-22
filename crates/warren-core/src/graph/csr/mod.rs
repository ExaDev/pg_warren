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
mod rows;
mod slices;
mod target;
mod transpose;

pub(crate) use any::AnyCsrSlices;
pub use any::{AnyCsr, Neighbours};
pub use build::{BuildPlan, BuildSummary, BuiltCsr, CsrDirection, CsrScope, CsrSink};
pub use graph_csr::{CsrLayout, GraphCsr, RelationCsr};
pub use rows::{AllRelations, CsrRows, RowNeighbours};
pub(crate) use slices::CsrSlices;
pub use target::{Target, TargetWidth};

use super::{DenseId, GraphError, SegmentBuffer};

/// The CSR of one relation in one direction, with targets of width `T`.
#[derive(Debug, Clone, PartialEq)]
pub struct Csr<T> {
    id_space: u64,
    offsets: SegmentBuffer<u64>,
    targets: SegmentBuffer<T>,
    weights: Option<SegmentBuffer<f64>>,
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
        Self::from_segments(id_space, offsets.into(), targets.into(), weights.map(Into::into))
    }

    /// [`Self::from_parts`] over segment buffers, owned or shared views,
    /// with the same checks.
    pub fn from_segments(
        id_space: u64,
        offsets: SegmentBuffer<u64>,
        targets: SegmentBuffer<T>,
        weights: Option<SegmentBuffer<f64>>,
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
        self.offsets.bytes()
            + self.targets.bytes()
            + self.weights.as_ref().map_or(0, SegmentBuffer::bytes)
    }

    /// The targets of `source`'s row in ascending order; empty outside the
    /// id space.
    pub fn neighbours(&self, source: DenseId) -> &[T] {
        self.slices().neighbours(source)
    }

    /// The weights of `source`'s row, aligned with [`Self::neighbours`].
    pub fn neighbour_weights(&self, source: DenseId) -> Option<&[f64]> {
        self.slices().neighbour_weights(source)
    }

    /// The number of targets in `source`'s row.
    pub fn degree(&self, source: DenseId) -> u64 {
        self.slices().degree(source)
    }

    /// Whether the edge `source → target` exists (binary search in the row).
    pub fn has_edge(&self, source: DenseId, target: DenseId) -> bool {
        T::from_dense(target)
            .is_some_and(|target| self.neighbours(source).binary_search(&target).is_ok())
    }
}

#[cfg(test)]
impl<T: Target> Csr<T> {
    /// The same CSR with every buffer a shared view of a heap segment.
    pub(crate) fn shared_views(&self) -> Self {
        Self {
            id_space: self.id_space,
            offsets: shared_view(&self.offsets),
            targets: shared_view(&self.targets),
            weights: self.weights.as_deref().map(shared_view),
        }
    }
}

/// A shared view of a heap segment holding a copy of `elements`.
#[cfg(test)]
fn shared_view<E: Clone + Send + Sync + 'static>(elements: &[E]) -> SegmentBuffer<E> {
    SegmentBuffer::shared(super::HeapSegment::of(elements))
}

#[cfg(test)]
mod tests {
    use super::shared_view as shared;
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

    #[test]
    fn a_csr_of_shared_views_reads_as_the_owned_one() {
        let owned = example();
        let shared = Csr::from_segments(
            3,
            shared(&[0, 2, 2, 4]),
            shared(&[1u32, 2, 0, 2]),
            Some(shared(&[0.5, 1.5, 2.5, 3.5])),
        )
        .unwrap();
        assert_eq!(shared, owned);
        assert_eq!(shared.neighbours(2), &[0, 2]);
        assert_eq!(shared.neighbour_weights(0), Some(&[0.5, 1.5][..]));
        assert_eq!(shared.bytes(), owned.bytes());
    }

    #[test]
    fn shared_views_are_validated_as_owned_buffers_are() {
        let short_targets = Csr::from_segments(3, shared(&[0, 2, 2, 4]), shared(&[1u32, 2]), None);
        assert_eq!(short_targets, Err(GraphError::FinalOffset { expected: 2, found: 4 }));
        let unsorted = Csr::from_segments(3, shared(&[0, 2, 2, 2]), shared(&[2u32, 1]), None);
        assert_eq!(unsorted, Err(GraphError::RowNotSorted { row: 0 }));
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
