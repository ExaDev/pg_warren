//! What `gather` returns: one row per receiver that got a message, in
//! ascending dense-id order, ready to stream to SQL.

use super::{Combine, Combiner};
use crate::compute::{ComputeError, ComputeMemory, MemoryCharge, Message, MessageBuffer};
use crate::graph::DenseId;

/// The receivers of a gather and their combined values, charged to the
/// compute memory budget. Receivers that got no message are absent.
#[derive(Debug)]
pub struct Gathered<M: Message> {
    combiner: Combiner,
    parts: Vec<Part<M>>,
}

impl<M: Message> Gathered<M> {
    /// The receivers of `parts`, which cover ascending runs of dense ids.
    pub(super) fn new(combiner: Combiner, mut parts: Vec<Part<M>>) -> Self {
        parts.retain(|part| !part.receivers.is_empty());
        Self { combiner, parts }
    }

    /// The combiner the values were combined with.
    pub fn combiner(&self) -> Combiner {
        self.combiner
    }

    /// The number of receivers.
    pub fn len(&self) -> usize {
        self.parts.iter().map(|part| part.receivers.len()).sum()
    }

    /// Whether no node received a message.
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Each receiver and its value, in ascending dense-id order.
    pub fn iter(&self) -> impl Iterator<Item = (DenseId, Received<'_, M>)> {
        let combiner = self.combiner;
        self.parts.iter().flat_map(move |part| {
            (0..part.receivers.len()).filter_map(move |index| {
                Some((part.receivers[index], part.received(combiner, index)?))
            })
        })
    }

    /// The bytes charged for the result.
    pub fn bytes(&self) -> u64 {
        self.parts.iter().map(|part| part.charge.bytes() + part.messages.bytes()).sum()
    }
}

/// A receiver's value.
#[derive(Debug, Clone, Copy)]
pub enum Received<'a, M: Message> {
    /// `count`: the number of messages received.
    Count(i64),
    /// `sum`, `min`, `max`, `first`, `concat`, `bool_or`, `bool_and`: one
    /// message of the messages' type.
    Message(M::View<'a>),
    /// `collect`: the messages received, in collect order.
    Messages(Collected<'a, M>),
}

/// The messages one receiver collected, each with its own boundaries.
#[derive(Debug, Clone, Copy)]
pub struct Collected<'a, M: Message> {
    messages: &'a MessageBuffer<M>,
    start: usize,
    end: usize,
}

impl<'a, M: Message> Collected<'a, M> {
    /// The number of messages.
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// Whether there are none (never, for a receiver in a [`Gathered`]).
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// The messages in collect order.
    pub fn iter(&self) -> impl Iterator<Item = M::View<'a>> + 'a {
        let messages = self.messages;
        (self.start..self.end).filter_map(move |index| messages.get(index))
    }
}

/// The receivers in one run of dense ids and their values.
#[derive(Debug)]
pub(super) struct Part<M: Message> {
    receivers: Vec<DenseId>,
    /// `count`: each receiver's count.
    counts: Vec<i64>,
    /// `collect`: receiver `i`'s messages end at `ends[i]`.
    ends: Vec<usize>,
    /// The messages: one per receiver, or every collected message.
    messages: MessageBuffer<M>,
    charge: MemoryCharge,
}

impl<M: Combine> Part<M> {
    /// No receivers yet.
    pub(super) fn new(memory: &ComputeMemory) -> Result<Self, ComputeError> {
        Ok(Self {
            receivers: Vec::new(),
            counts: Vec::new(),
            ends: Vec::new(),
            messages: MessageBuffer::new(memory)?,
            charge: memory.charge(0)?,
        })
    }

    /// Combines the messages `receiver` received, in collect order, adding
    /// the receiver if there are any.
    pub(super) fn receive<'a>(
        &mut self,
        combiner: Combiner,
        receiver: DenseId,
        mut received: impl Iterator<Item = M::View<'a>>,
    ) -> Result<(), ComputeError> {
        let any = match combiner {
            Combiner::Count => {
                let count = received.count();
                if count > 0 {
                    self.charge.reserve_one(&mut self.counts)?;
                    self.counts.push(count as i64);
                }
                count > 0
            }
            Combiner::Collect => {
                let start = self.messages.len();
                for message in received {
                    self.messages.push(message)?;
                }
                let any = self.messages.len() > start;
                if any {
                    self.charge.reserve_one(&mut self.ends)?;
                    self.ends.push(self.messages.len());
                }
                any
            }
            Combiner::First => match received.next() {
                Some(first) => self.messages.push(first).map(|()| true)?,
                None => false,
            },
            Combiner::Concat => M::concat(&mut self.messages, received)?,
            Combiner::Sum
            | Combiner::Min
            | Combiner::Max
            | Combiner::BoolOr
            | Combiner::BoolAnd => {
                let Some(mut value) = received.next() else { return Ok(()) };
                for message in received {
                    value = M::fold(combiner, value, message)?;
                }
                self.messages.push(value).map(|()| true)?
            }
        };
        if any {
            self.charge.reserve_one(&mut self.receivers)?;
            self.receivers.push(receiver);
        }
        Ok(())
    }
}

impl<M: Message> Part<M> {
    fn received(&self, combiner: Combiner, index: usize) -> Option<Received<'_, M>> {
        Some(match combiner {
            Combiner::Count => Received::Count(*self.counts.get(index)?),
            Combiner::Collect => Received::Messages(Collected {
                messages: &self.messages,
                start: match index {
                    0 => 0,
                    index => *self.ends.get(index - 1)?,
                },
                end: *self.ends.get(index)?,
            }),
            _ => Received::Message(self.messages.get(index)?),
        })
    }
}
