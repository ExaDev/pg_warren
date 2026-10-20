//! The messages `gather` fans out: a message per sending node, or several,
//! for any subset of the graph's nodes.

use crate::compute::{ComputeError, ComputeMemory, MemoryCharge, Message, MessageBuffer};
use crate::graph::DenseId;

/// Messages and their senders' dense ids, in the order they were pushed,
/// charged to the compute memory budget. A sender may send several
/// messages; nodes that send none are simply absent.
#[derive(Debug)]
pub struct Messages<M: Message> {
    senders: Vec<DenseId>,
    buffer: MessageBuffer<M>,
    charge: MemoryCharge,
}

impl<M: Message> Messages<M> {
    /// No messages, charging `memory`.
    pub fn new(memory: &ComputeMemory) -> Result<Self, ComputeError> {
        Ok(Self {
            senders: Vec::new(),
            buffer: MessageBuffer::new(memory)?,
            charge: memory.charge(0)?,
        })
    }

    /// Appends `message` from `sender`, charging the capacity reserved for
    /// each first. A failed push keeps no message; capacity it reserved
    /// stays charged.
    pub fn push(&mut self, sender: DenseId, message: M::View<'_>) -> Result<(), ComputeError> {
        self.charge.reserve_one(&mut self.senders)?;
        self.buffer.push(message)?;
        self.senders.push(sender);
        Ok(())
    }

    /// The number of messages.
    pub fn len(&self) -> usize {
        self.senders.len()
    }

    /// Whether there are no messages.
    pub fn is_empty(&self) -> bool {
        self.senders.is_empty()
    }

    /// `(sender, message)` in the order they were pushed.
    pub fn iter(&self) -> impl Iterator<Item = (DenseId, M::View<'_>)> {
        self.senders.iter().copied().zip(self.buffer.iter())
    }

    /// Every message's sender, in push order.
    pub(super) fn senders(&self) -> &[DenseId] {
        &self.senders
    }

    /// The message at `index`.
    pub(super) fn message(&self, index: usize) -> Option<M::View<'_>> {
        self.buffer.get(index)
    }

    /// The bytes charged for the messages alone, without their senders.
    pub(super) fn message_bytes(&self) -> u64 {
        self.buffer.bytes()
    }

    /// The bytes charged for the messages and their senders.
    pub fn bytes(&self) -> u64 {
        self.charge.bytes() + self.buffer.bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute::{Bytes, ComputeError, ComputeMemory};

    #[test]
    fn messages_keep_their_senders_and_are_charged() {
        let memory = ComputeMemory::new(1024);
        let mut messages = Messages::<Bytes>::new(&memory).unwrap();
        assert!(messages.is_empty());
        messages.push(4, b"ab").unwrap();
        messages.push(1, b"").unwrap();
        messages.push(4, b"c").unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages.iter().collect::<Vec<_>>(), [(4, &b"ab"[..]), (1, b""), (4, b"c")]);
        // The charge is the reserved capacity: 4 senders and 4 end offsets
        // of 8 bytes, and 4 bytes (2, then doubled).
        assert_eq!(messages.bytes(), 4 * 8 + 4 * 8 + 4);
        assert_eq!(memory.in_use(), messages.bytes());
        drop(messages);
        assert_eq!(memory.in_use(), 0);
    }

    #[test]
    fn a_message_past_the_budget_is_an_error_and_is_not_kept() {
        let memory = ComputeMemory::new(20);
        let mut messages = Messages::<f64>::new(&memory).unwrap();
        messages.push(0, 1.0).unwrap();
        assert!(matches!(messages.push(1, 2.0), Err(ComputeError::MemoryLimit { .. })));
        assert_eq!(messages.iter().collect::<Vec<_>>(), [(0, 1.0)]);
        assert_eq!(memory.in_use(), 16);
        // Room for the sender, but not for the message.
        let memory = ComputeMemory::new(24);
        let mut messages = Messages::<f64>::new(&memory).unwrap();
        messages.push(0, 1.0).unwrap();
        assert!(messages.push(1, 2.0).is_err());
        // The senders' second slot stays reserved, and charged.
        assert_eq!((messages.len(), memory.in_use()), (1, 24));
    }
}
