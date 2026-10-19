//! Each sender's messages by dense id: a stable counting sort of the
//! messages by sender, so a sender's messages keep their push order.

use super::Messages;
use crate::compute::{ComputeError, ComputeMemory, MemoryCharge, Message};
use crate::graph::DenseId;
use crate::interrupts::{Cancellation, Cancelled};

/// How often a sequential pass checks the cancellation flag.
pub(super) const ROWS_PER_CHECK: usize = 1 << 16;

/// The messages of each sender, as indexes into [`Messages`].
#[derive(Debug)]
pub(super) struct Outbox {
    /// `id_space + 1` positions into `order`: sender `s`'s messages are
    /// `order[offsets[s]..offsets[s + 1]]`.
    offsets: Vec<u64>,
    order: Vec<usize>,
    _charges: [MemoryCharge; 2],
}

impl Outbox {
    /// Sorts `messages` by sender, failing on a sender outside `id_space`.
    pub(super) fn new<M: Message>(
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
        let mut end = 0;
        for offset in &mut offsets[..id_space as usize] {
            end += *offset;
            *offset = end;
        }
        offsets[id_space as usize] = end;
        let (mut order, order_charge) = memory.filled(senders.len(), 0usize)?;
        for (index, &sender) in senders.iter().enumerate().rev() {
            check(cancellation, index)?;
            let offset = &mut offsets[sender as usize];
            *offset -= 1;
            order[*offset as usize] = index;
        }
        Ok(Self { offsets, order, _charges: [offsets_charge, order_charge] })
    }

    /// The indexes of `sender`'s messages, in push order.
    pub(super) fn of(&self, sender: DenseId) -> &[usize] {
        let sender = sender as usize;
        &self.order[self.offsets[sender] as usize..self.offsets[sender + 1] as usize]
    }
}

/// Checks the cancellation flag every [`ROWS_PER_CHECK`] rows.
pub(super) fn check(cancellation: &Cancellation, row: usize) -> Result<(), Cancelled> {
    match row.is_multiple_of(ROWS_PER_CHECK) && cancellation.is_cancelled() {
        true => Err(Cancelled),
        false => Ok(()),
    }
}
