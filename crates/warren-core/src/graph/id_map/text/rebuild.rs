//! The keys of a text rebuild (DESIGN.md §5.7). Dense ids are stable and
//! `id_bytes` and `id_offsets` are in dense-id order, so the new arrays are
//! the base's with the fresh ids appended; only `id_order` is built anew,
//! by the merge walk, as a merge of the base order with the fresh ids.

use super::order::OrderBuffer;
use super::TextKeys;
use crate::graph::build_memory::bytes_of;
use crate::graph::{
    memory, BuildBuffer, BuildCharge, BuildMemory, DenseId, FreshKeys, GraphError, IdKeys,
    KeysRebuild, SegmentBuffer,
};

/// The keys of a text rebuild: the new order, and the fresh ids' bytes and
/// ends in dense-id order, each reserved exactly.
#[derive(Debug)]
pub struct TextRebuild {
    order: OrderBuffer,
    fresh_bytes: BuildBuffer<u8>,
    /// Where each fresh id ends, from the start of `fresh_bytes`.
    fresh_ends: BuildBuffer<u64>,
}

impl TextRebuild {
    /// Room for the rebuild of `base` that adds `fresh`, charged to
    /// `memory`.
    pub(super) fn new(
        base: &TextKeys,
        fresh: FreshKeys,
        memory: &BuildMemory,
    ) -> Result<Self, GraphError> {
        let fresh_bytes = usize::try_from(fresh.bytes)
            .map_err(|_| GraphError::OutOfMemory { elements: usize::MAX })?;
        Ok(Self {
            order: OrderBuffer::for_id_space(base.id_space() + fresh.count as u64, memory)?,
            fresh_bytes: BuildBuffer::with_capacity(memory, fresh_bytes)?,
            fresh_ends: BuildBuffer::with_capacity(memory, fresh.count)?,
        })
    }

    /// The rebuilt keys: the base's bytes and offsets, each extended by
    /// the fresh ids, and the new order; and the charge for what it
    /// allocated.
    fn into_keys(
        self,
        base_bytes: Base<'_, u8>,
        base_offsets: Base<'_, u64>,
    ) -> Result<(TextKeys, BuildCharge), GraphError> {
        let Self { order, fresh_bytes, fresh_ends } = self;
        let memory = fresh_ends.memory().clone();
        let end = base_bytes.len() as u64;
        let (bytes, mut charge) = extended(base_bytes, fresh_bytes.iter().copied(), &memory)?;
        drop(fresh_bytes);
        let fresh_offsets = fresh_ends.iter().map(|fresh_end| end + fresh_end);
        let (offsets, offsets_charge) = extended(base_offsets, fresh_offsets, &memory)?;
        drop(fresh_ends);
        let (order, order_charge) = order.into_parts();
        charge.absorb(offsets_charge);
        charge.absorb(order_charge);
        Ok((TextKeys { bytes: bytes.into(), offsets: offsets.into(), order }, charge))
    }
}

impl KeysRebuild<TextKeys> for TextRebuild {
    fn keep(&mut self, _: &[u8], dense: DenseId) -> Result<(), GraphError> {
        self.order.push(dense)
    }

    /// Fresh ids arrive in id order, which is their dense-id order.
    fn add(&mut self, key: &[u8], dense: DenseId) -> Result<(), GraphError> {
        self.order.push(dense)?;
        self.fresh_bytes.extend(key.iter().copied());
        self.fresh_ends.push(self.fresh_bytes.len() as u64)
    }

    /// Copies the base's bytes and offsets, with the fresh ids appended,
    /// into buffers reserved exactly.
    fn finish(self, base: &TextKeys) -> Result<(TextKeys, BuildCharge), GraphError> {
        self.into_keys(Base::Borrowed(&base.bytes), Base::Borrowed(&base.offsets))
    }

    /// Extends the base's bytes and offsets in place
    /// (`try_reserve_exact`), unless they are shared views, which are
    /// copied; the base's order is freed first.
    fn finish_into(self, base: TextKeys) -> Result<(TextKeys, BuildCharge), GraphError> {
        let TextKeys { bytes, offsets, order } = base;
        drop(order);
        self.into_keys(Base::Owned(bytes), Base::Owned(offsets))
    }
}

/// A base array a rebuild extends.
enum Base<'a, T> {
    /// Read, so copied.
    Borrowed(&'a [T]),
    /// Consumed, so extended in place if it is owned.
    Owned(SegmentBuffer<T>),
}

impl<T> Base<'_, T> {
    fn len(&self) -> usize {
        match self {
            Self::Borrowed(base) => base.len(),
            Self::Owned(base) => base.len(),
        }
    }
}

/// `base` followed by `more`, and the charge for what that allocated: an
/// owned buffer grows in place by exactly `more`, anything else is copied
/// into a buffer reserved exactly.
fn extended<T: Copy>(
    base: Base<'_, T>,
    more: impl ExactSizeIterator<Item = T>,
    budget: &BuildMemory,
) -> Result<(Vec<T>, BuildCharge), GraphError> {
    let owned = match base {
        Base::Owned(base) => base.into_owned().map_err(Base::Owned),
        borrowed => Err(borrowed),
    };
    match owned {
        Ok(mut buffer) => {
            let charge = budget.charge(bytes_of::<T>(more.len()))?;
            buffer
                .try_reserve_exact(more.len())
                .map_err(|_| GraphError::OutOfMemory { elements: buffer.len() + more.len() })?;
            buffer.extend(more);
            Ok((buffer, charge))
        }
        Err(base) => {
            let base: &[T] = match &base {
                Base::Borrowed(base) => base,
                Base::Owned(base) => base,
            };
            let length = base.len() + more.len();
            let charge = budget.charge(bytes_of::<T>(length))?;
            let mut buffer = memory::with_capacity(length)?;
            buffer.extend_from_slice(base);
            buffer.extend(more);
            Ok((buffer, charge))
        }
    }
}
