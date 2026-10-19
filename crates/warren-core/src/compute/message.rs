//! Messages: what `gather` fans out over the graph (DESIGN.md §6.1, §6.4).
//!
//! Message buffers are generic over [`Message`]: a fixed-width element type
//! is the fast path, stored as a plain array; [`Bytes`] is the general path
//! for messages of any SQL type, stored packed as `(offsets, bytes)`.

use std::collections::TryReserveError;
use std::fmt::Debug;
use std::mem::size_of;

use super::{ComputeError, ComputeMemory, Element, MemoryCharge};

/// A message type: how a buffer stores messages and hands them back.
pub trait Message {
    /// A message as it is pushed and read back.
    type View<'a>: Copy;
    /// Where a buffer keeps its messages.
    type Storage: Default + Debug + Send + Sync;

    /// The bytes [`Message::push`] will reserve beyond what `storage` has
    /// reserved, to make room for `message`.
    fn growth(storage: &Self::Storage, message: Self::View<'_>) -> u64;

    /// Appends `message`, reserving exactly the [`Message::growth`] with
    /// `try_reserve_exact`.
    fn push(storage: &mut Self::Storage, message: Self::View<'_>) -> Result<(), TryReserveError>;

    /// The message at `index`.
    fn get(storage: &Self::Storage, index: usize) -> Option<Self::View<'_>>;

    /// The number of messages.
    fn len(storage: &Self::Storage) -> usize;
}

/// The fast path: one element per message, in a plain array.
impl<E: Element> Message for E {
    type View<'a> = E;
    type Storage = Vec<E>;

    fn growth(storage: &Vec<E>, _: E) -> u64 {
        growth(storage, 1)
    }

    fn push(storage: &mut Vec<E>, message: E) -> Result<(), TryReserveError> {
        reserve(storage, 1)?;
        storage.push(message);
        Ok(())
    }

    fn get(storage: &Vec<E>, index: usize) -> Option<E> {
        storage.get(index).copied()
    }

    fn len(storage: &Vec<E>) -> usize {
        storage.len()
    }
}

/// Variable-width messages: byte strings, such as serialised SQL values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bytes;

/// Byte-string messages packed back to back: message `i` is
/// `bytes[ends[i - 1]..ends[i]]`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PackedBytes {
    ends: Vec<u64>,
    bytes: Vec<u8>,
}

impl Message for Bytes {
    type View<'a> = &'a [u8];
    type Storage = PackedBytes;

    fn growth(storage: &PackedBytes, message: &[u8]) -> u64 {
        growth(&storage.ends, 1) + growth(&storage.bytes, message.len())
    }

    fn push(storage: &mut PackedBytes, message: &[u8]) -> Result<(), TryReserveError> {
        reserve(&mut storage.ends, 1)?;
        reserve(&mut storage.bytes, message.len())?;
        storage.bytes.extend_from_slice(message);
        storage.ends.push(storage.bytes.len() as u64);
        Ok(())
    }

    fn get(storage: &PackedBytes, index: usize) -> Option<&[u8]> {
        let end = *storage.ends.get(index)? as usize;
        let start = match index {
            0 => 0,
            index => storage.ends[index - 1] as usize,
        };
        storage.bytes.get(start..end)
    }

    fn len(storage: &PackedBytes) -> usize {
        storage.ends.len()
    }
}

/// The capacity `buffer` grows to for `additional` more elements: unchanged
/// if they fit, else doubled (or just enough, if more).
fn grown_capacity<T>(buffer: &Vec<T>, additional: usize) -> usize {
    let needed = buffer.len().saturating_add(additional);
    match needed <= buffer.capacity() {
        true => buffer.capacity(),
        false => needed.max(buffer.capacity().saturating_mul(2)),
    }
}

/// The bytes `buffer` reserves beyond its capacity to fit `additional`.
fn growth<T>(buffer: &Vec<T>, additional: usize) -> u64 {
    let elements = grown_capacity(buffer, additional) - buffer.capacity();
    (elements as u64).saturating_mul(size_of::<T>() as u64)
}

/// Grows `buffer` to exactly [`grown_capacity`].
fn reserve<T>(buffer: &mut Vec<T>, additional: usize) -> Result<(), TryReserveError> {
    let target = grown_capacity(buffer, additional);
    buffer.try_reserve_exact(target - buffer.len())
}

/// An append-only buffer of messages, charged to the compute memory budget.
#[derive(Debug)]
pub struct MessageBuffer<M: Message> {
    storage: M::Storage,
    charge: MemoryCharge,
}

impl<M: Message> MessageBuffer<M> {
    /// An empty buffer charging `memory`.
    pub fn new(memory: &ComputeMemory) -> Result<Self, ComputeError> {
        Ok(Self { storage: M::Storage::default(), charge: memory.charge(0)? })
    }

    /// Appends `message`, charging any capacity it reserves first, so the
    /// charge is always the buffer's reserved capacity.
    pub fn push(&mut self, message: M::View<'_>) -> Result<(), ComputeError> {
        let size = M::growth(&self.storage, message);
        self.charge.grow(size)?;
        M::push(&mut self.storage, message).map_err(|_| {
            self.charge.shrink(size);
            ComputeError::OutOfMemory { bytes: size }
        })
    }

    /// The message at `index`.
    pub fn get(&self, index: usize) -> Option<M::View<'_>> {
        M::get(&self.storage, index)
    }

    /// The number of messages.
    pub fn len(&self) -> usize {
        M::len(&self.storage)
    }

    /// Whether the buffer holds no messages.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The messages in the order they were pushed.
    pub fn iter(&self) -> impl Iterator<Item = M::View<'_>> {
        (0..self.len()).filter_map(|index| self.get(index))
    }

    /// The bytes charged: the buffer's reserved capacity.
    pub fn bytes(&self) -> u64 {
        self.charge.bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_width_messages_come_back_in_push_order() {
        let memory = ComputeMemory::new(1024);
        let mut buffer = MessageBuffer::<f64>::new(&memory).unwrap();
        assert!(buffer.is_empty());
        for message in [0.5, -1.0, 2.0] {
            buffer.push(message).unwrap();
        }
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.get(1), Some(-1.0));
        assert_eq!(buffer.get(3), None);
        assert_eq!(buffer.iter().collect::<Vec<_>>(), [0.5, -1.0, 2.0]);
        // The charge is the reserved capacity: 1, 2, then 4 elements.
        assert_eq!((buffer.bytes(), memory.in_use()), (32, 32));
        drop(buffer);
        assert_eq!(memory.in_use(), 0);
    }

    #[test]
    fn variable_width_messages_are_packed_as_offsets_and_bytes() {
        let memory = ComputeMemory::new(1024);
        let mut buffer = MessageBuffer::<Bytes>::new(&memory).unwrap();
        for message in [&b"alice"[..], b"", b"bob"] {
            buffer.push(message).unwrap();
        }
        assert_eq!(buffer.iter().collect::<Vec<_>>(), [&b"alice"[..], b"", b"bob"]);
        assert_eq!(buffer.get(2), Some(&b"bob"[..]));
        assert_eq!(buffer.get(3), None);
        // Reserved: 4 end offsets of 8 bytes, and 10 bytes (5, then doubled).
        assert_eq!(buffer.bytes(), 4 * 8 + 10);
    }

    #[test]
    fn a_message_past_the_budget_is_an_error_and_is_not_kept() {
        let memory = ComputeMemory::new(20);
        let mut buffer = MessageBuffer::<Bytes>::new(&memory).unwrap();
        buffer.push(b"0123456789").unwrap();
        assert!(matches!(buffer.push(b"x"), Err(ComputeError::MemoryLimit { .. })));
        assert_eq!(buffer.len(), 1);
        assert_eq!(memory.in_use(), 18);
        let mut integers = MessageBuffer::<i64>::new(&memory).unwrap();
        assert!(integers.push(1).is_err());
    }
}
