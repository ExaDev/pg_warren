//! Messages: what `gather` fans out over the graph (DESIGN.md §6.1, §6.4).
//!
//! Message buffers are generic over [`Message`]: a fixed-width element type
//! is the fast path, stored as a plain array; [`Bytes`] is the general path
//! for messages of any SQL type, stored packed as `(offsets, bytes)`.

use std::collections::TryReserveError;
use std::fmt::{self, Debug, Display};
use std::mem::size_of;

use super::{ComputeError, ComputeMemory, Dtype, Element, MemoryCharge};

/// What a message type holds, as type errors name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageKind {
    /// One vector element per message: the fast path.
    Element(Dtype),
    /// A byte string per message: the general path.
    Bytes,
}

impl Display for MessageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Element(dtype) => Display::fmt(dtype, f),
            Self::Bytes => f.write_str("bytes"),
        }
    }
}

/// A message type: how a buffer stores messages and hands them back.
pub trait Message {
    /// What the messages hold.
    const KIND: MessageKind;
    /// A message as it is pushed and read back.
    type View<'a>: Copy + Debug;
    /// Where a buffer keeps its messages.
    type Storage: Default + Debug + Send + Sync;

    /// The bytes `message` adds to a buffer.
    fn size(message: Self::View<'_>) -> u64;

    /// Appends `message`, reserving with `try_reserve`.
    fn push(storage: &mut Self::Storage, message: Self::View<'_>) -> Result<(), TryReserveError>;

    /// The message at `index`.
    fn get(storage: &Self::Storage, index: usize) -> Option<Self::View<'_>>;

    /// The number of messages.
    fn len(storage: &Self::Storage) -> usize;
}

/// The fast path: one element per message, in a plain array.
impl<E: Element> Message for E {
    const KIND: MessageKind = MessageKind::Element(E::DTYPE);
    type View<'a> = E;
    type Storage = Vec<E>;

    fn size(_: E) -> u64 {
        size_of::<E>() as u64
    }

    fn push(storage: &mut Vec<E>, message: E) -> Result<(), TryReserveError> {
        storage.try_reserve(1)?;
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
    const KIND: MessageKind = MessageKind::Bytes;
    type View<'a> = &'a [u8];
    type Storage = PackedBytes;

    fn size(message: &[u8]) -> u64 {
        message.len() as u64 + size_of::<u64>() as u64
    }

    fn push(storage: &mut PackedBytes, message: &[u8]) -> Result<(), TryReserveError> {
        storage.ends.try_reserve(1)?;
        storage.bytes.try_reserve(message.len())?;
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

    /// Appends `message`, charging its size first.
    pub fn push(&mut self, message: M::View<'_>) -> Result<(), ComputeError> {
        let size = M::size(message);
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

    /// The bytes charged for the messages.
    pub fn bytes(&self) -> u64 {
        self.charge.bytes()
    }
}

impl MessageBuffer<Bytes> {
    /// Appends one message, the concatenation of `parts`, charging each
    /// part before it is reserved. Appends nothing if there are no parts;
    /// returns whether there were any. On an error nothing is kept.
    pub(crate) fn push_concatenation<'a>(
        &mut self,
        parts: impl Iterator<Item = &'a [u8]>,
    ) -> Result<bool, ComputeError> {
        let (bytes, charged) = (self.storage.bytes.len(), self.charge.bytes());
        let appended = self.append(parts);
        if appended.is_err() {
            self.storage.bytes.truncate(bytes);
            self.charge.shrink(self.charge.bytes() - charged);
        }
        appended
    }

    fn append<'a>(&mut self, parts: impl Iterator<Item = &'a [u8]>) -> Result<bool, ComputeError> {
        let mut any = false;
        for part in parts {
            // The first part also pays for the message's end offset.
            let size = if any { part.len() as u64 } else { Bytes::size(part) };
            self.charge.grow(size)?;
            let storage = &mut self.storage;
            let reserved = match any {
                true => storage.bytes.try_reserve(part.len()),
                false => {
                    storage.ends.try_reserve(1).and_then(|()| storage.bytes.try_reserve(part.len()))
                }
            };
            reserved.map_err(|_| ComputeError::OutOfMemory { bytes: size })?;
            storage.bytes.extend_from_slice(part);
            any = true;
        }
        if any {
            self.storage.ends.push(self.storage.bytes.len() as u64);
        }
        Ok(any)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_type_names_its_kind() {
        assert_eq!(<f32 as Message>::KIND, MessageKind::Element(Dtype::F32));
        assert_eq!(<bool as Message>::KIND.to_string(), "bool");
        assert_eq!(Bytes::KIND, MessageKind::Bytes);
        assert_eq!(Bytes::KIND.to_string(), "bytes");
    }

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
        assert_eq!((buffer.bytes(), memory.in_use()), (24, 24));
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
        // Each message costs its bytes plus its 8-byte end offset.
        assert_eq!(buffer.bytes(), 5 + 3 + 3 * 8);
    }

    #[test]
    fn a_concatenation_is_one_message_charged_like_one() {
        let memory = ComputeMemory::new(30);
        let mut buffer = MessageBuffer::<Bytes>::new(&memory).unwrap();
        assert_eq!(buffer.push_concatenation([&b"ab"[..], b"", b"c"].into_iter()), Ok(true));
        assert_eq!(buffer.push_concatenation(std::iter::empty()), Ok(false));
        assert_eq!(buffer.iter().collect::<Vec<_>>(), [&b"abc"[..]]);
        assert_eq!(buffer.bytes(), 3 + 8);
        let too_long = [&b"0123456789"[..], b"0123456789"];
        assert!(buffer.push_concatenation(too_long.into_iter()).is_err());
        assert_eq!(buffer.iter().collect::<Vec<_>>(), [&b"abc"[..]]);
        assert_eq!((buffer.bytes(), memory.in_use()), (11, 11));
        buffer.push(b"d").unwrap();
        assert_eq!(buffer.get(1), Some(&b"d"[..]));
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
