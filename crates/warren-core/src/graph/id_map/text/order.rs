//! `id_order`: a text id map's dense ids sorted by their ids, at the
//! version's target width.

use crate::graph::{
    memory, BuildBuffer, BuildCharge, BuildMemory, DenseId, GraphError, Permutation, SegmentBuffer,
    Target, TargetWidth,
};

/// The dense ids of a text id map in id order: `u32` when the id space is
/// below 2³², else `u64`, as a version's CSR targets are (DESIGN.md §5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdOrder {
    /// 4-byte dense ids.
    U32(SegmentBuffer<u32>),
    /// 8-byte dense ids.
    U64(SegmentBuffer<u64>),
}

impl Default for IdOrder {
    fn default() -> Self {
        Self::U32(SegmentBuffer::default())
    }
}

impl IdOrder {
    /// The width of its dense ids.
    pub fn width(&self) -> TargetWidth {
        match self {
            Self::U32(_) => TargetWidth::U32,
            Self::U64(_) => TargetWidth::U64,
        }
    }

    /// The number of dense ids.
    pub fn len(&self) -> usize {
        match self {
            Self::U32(order) => order.len(),
            Self::U64(order) => order.len(),
        }
    }

    /// Whether it holds no dense id.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The heap bytes it holds.
    pub fn bytes(&self) -> u64 {
        match self {
            Self::U32(order) => order.bytes(),
            Self::U64(order) => order.bytes(),
        }
    }

    /// The dense id at `rank`, the position in id order.
    pub fn get(&self, rank: usize) -> Option<DenseId> {
        match self {
            Self::U32(order) => order.get(rank).map(|&dense| dense.dense()),
            Self::U64(order) => order.get(rank).copied(),
        }
    }

    /// The identity order of `len` dense ids, at their width.
    pub(super) fn identity(len: u64) -> Result<Self, GraphError> {
        fn identity<T: Target>(len: u64) -> Result<Vec<T>, GraphError> {
            let capacity = usize::try_from(len).map_err(|_| GraphError::IdSpaceTooLarge)?;
            let mut order = memory::with_capacity(capacity)?;
            for dense in 0..len {
                order.push(T::from_dense(dense).ok_or(GraphError::IdSpaceExceedsTargetWidth)?);
            }
            Ok(order)
        }
        Ok(match TargetWidth::for_id_space(len) {
            TargetWidth::U32 => Self::U32(identity(len)?.into()),
            TargetWidth::U64 => Self::U64(identity(len)?.into()),
        })
    }

    /// This order with every dense id moved by `permutation`, charged to
    /// `memory` while it is held. It stays sorted, since it is sorted by
    /// id, not by dense id.
    pub(super) fn renumbered(
        &self,
        permutation: &Permutation,
        memory: &BuildMemory,
    ) -> Result<(Self, BuildCharge), GraphError> {
        fn renumbered<T: Target>(
            order: &[T],
            permutation: &Permutation,
            memory: &BuildMemory,
        ) -> Result<(Vec<T>, BuildCharge), GraphError> {
            let mut new = BuildBuffer::with_capacity(memory, order.len())?;
            for old in order {
                let dense = permutation.new_dense_id(old.dense());
                new.push(T::from_dense(dense).ok_or(GraphError::IdSpaceExceedsTargetWidth)?)?;
            }
            Ok(new.into_parts())
        }
        Ok(match self {
            Self::U32(order) => {
                let (order, charge) = renumbered(order, permutation, memory)?;
                (Self::U32(order.into()), charge)
            }
            Self::U64(order) => {
                let (order, charge) = renumbered(order, permutation, memory)?;
                (Self::U64(order.into()), charge)
            }
        })
    }
}

/// An order being built, at the width of the id space it will cover, with
/// room reserved for all of it.
#[derive(Debug)]
pub(super) enum OrderBuffer {
    /// 4-byte dense ids.
    U32(BuildBuffer<u32>),
    /// 8-byte dense ids.
    U64(BuildBuffer<u64>),
}

impl OrderBuffer {
    /// Room for the order of `id_space` dense ids, charged to `memory`.
    pub(super) fn for_id_space(id_space: u64, memory: &BuildMemory) -> Result<Self, GraphError> {
        let len = usize::try_from(id_space).map_err(|_| GraphError::IdSpaceTooLarge)?;
        Ok(match TargetWidth::for_id_space(id_space) {
            TargetWidth::U32 => Self::U32(BuildBuffer::with_capacity(memory, len)?),
            TargetWidth::U64 => Self::U64(BuildBuffer::with_capacity(memory, len)?),
        })
    }

    /// Appends the next dense id in id order.
    pub(super) fn push(&mut self, dense: DenseId) -> Result<(), GraphError> {
        fn push<T: Target>(order: &mut BuildBuffer<T>, dense: DenseId) -> Result<(), GraphError> {
            order.push(T::from_dense(dense).ok_or(GraphError::IdSpaceExceedsTargetWidth)?)
        }
        match self {
            Self::U32(order) => push(order, dense),
            Self::U64(order) => push(order, dense),
        }
    }

    /// The order and its charge.
    pub(super) fn into_parts(self) -> (IdOrder, BuildCharge) {
        match self {
            Self::U32(order) => {
                let (order, charge) = order.into_parts();
                (IdOrder::U32(order.into()), charge)
            }
            Self::U64(order) => {
                let (order, charge) = order.into_parts();
                (IdOrder::U64(order.into()), charge)
            }
        }
    }
}
