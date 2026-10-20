//! Each sender's messages by dense id: the messages copied in sender order
//! by a stable counting sort, so a sender's messages are adjacent and keep
//! their push order.

use std::mem::size_of;
use std::ops::Range;

use super::Messages;
use crate::compute::{ComputeError, ComputeMemory, MemoryCharge, Message, MessageBuffer};
use crate::graph::DenseId;
use crate::interrupts::{Cancellation, Cancelled};

/// How often a sequential pass checks the cancellation flag.
const ROWS_PER_CHECK: usize = 1 << 16;

/// The messages, grouped by sender.
#[derive(Debug)]
pub(super) struct Outbox<M: Message> {
    /// `id_space + 1` positions into `messages`: sender `s`'s messages are
    /// `offsets[s]..offsets[s + 1]`.
    offsets: Vec<u64>,
    messages: MessageBuffer<M>,
    _offsets: MemoryCharge,
}

impl<M: Message> Outbox<M> {
    /// About the bytes [`Self::new`] charges for `messages` over `id_space`
    /// nodes: its offsets, the sort order and the sorted copy.
    pub(super) fn bytes(messages: &Messages<M>, id_space: u64) -> u64 {
        let offsets = id_space.saturating_add(1).saturating_mul(size_of::<u64>() as u64);
        let order = (messages.len() as u64).saturating_mul(size_of::<usize>() as u64);
        offsets.saturating_add(order).saturating_add(messages.bytes())
    }

    /// Sorts `messages` by sender, failing on a sender outside `id_space`.
    pub(super) fn new(
        memory: &ComputeMemory,
        cancellation: &Cancellation,
        messages: &Messages<M>,
        id_space: u64,
    ) -> Result<Self, ComputeError> {
        let senders = messages.senders();
        let (mut offsets, offsets_charge) = memory.filled(id_space as usize + 1, 0u64)?;
        for (index, &sender) in senders.iter().enumerate() {
            check(cancellation, index)?;
            if sender >= id_space {
                return Err(ComputeError::SenderOutsideIdSpace { sender, id_space });
            }
            offsets[sender as usize] += 1;
        }
        // Running sums make `offsets[s]` the end of sender `s`'s messages;
        // placing the messages last to first then moves it to their start.
        running_ends(&mut offsets);
        let (mut order, _order) = memory.filled(senders.len(), 0usize)?;
        for (index, &sender) in senders.iter().enumerate().rev() {
            check(cancellation, index)?;
            let offset = &mut offsets[sender as usize];
            *offset -= 1;
            order[*offset as usize] = index;
        }
        let mut sorted = MessageBuffer::new(memory)?;
        for (position, &index) in order.iter().enumerate() {
            check(cancellation, position)?;
            if let Some(message) = messages.message(index) {
                sorted.push(message)?;
            }
        }
        Ok(Self { offsets, messages: sorted, _offsets: offsets_charge })
    }

    /// The positions of `sender`'s messages.
    fn range(&self, sender: DenseId) -> Range<usize> {
        let sender = sender as usize;
        self.offsets[sender] as usize..self.offsets[sender + 1] as usize
    }

    /// Whether `sender` sends any message.
    pub(super) fn sends(&self, sender: DenseId) -> bool {
        !self.range(sender).is_empty()
    }

    /// `sender`'s messages, in push order.
    pub(super) fn of(&self, sender: DenseId) -> impl Iterator<Item = M::View<'_>> + Clone {
        self.range(sender).filter_map(|index| self.messages.get(index))
    }
}

/// Turns per-row counts in `offsets[..n]` into running ends, and sets
/// `offsets[n]` to the total.
pub(super) fn running_ends(offsets: &mut [u64]) {
    let mut end = 0;
    if let Some((total, counts)) = offsets.split_last_mut() {
        for offset in counts {
            end += *offset;
            *offset = end;
        }
        *total = end;
    }
}

/// Checks the cancellation flag every [`ROWS_PER_CHECK`] rows.
pub(super) fn check(cancellation: &Cancellation, row: usize) -> Result<(), Cancelled> {
    match row.is_multiple_of(ROWS_PER_CHECK) && cancellation.is_cancelled() {
        true => Err(Cancelled),
        false => Ok(()),
    }
}
