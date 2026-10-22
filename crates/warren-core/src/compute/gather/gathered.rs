//! What `gather` returns: one row per receiver that got a message, in
//! ascending dense-id order, ready to stream to SQL.

use super::tally::Tally;
use super::{Combine, Combiner};
use crate::compute::memory::Allowance;
use crate::compute::{ComputeError, ComputeMemory, Message};
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

    /// The row at `at`, advancing `at` past it: a cursor for callers that
    /// own the result and stream it a row at a time, as `iter` yields.
    pub fn next_row(&self, at: &mut RowAt) -> Option<(DenseId, Received<'_, M>)> {
        loop {
            let part = self.parts.get(at.part)?;
            if at.index < part.receivers.len() {
                let index = at.index;
                at.index += 1;
                return Some((part.receivers[index], part.received(self.combiner, index)?));
            }
            at.part += 1;
            at.index = 0;
        }
    }

    /// The bytes charged for the result.
    pub fn bytes(&self) -> u64 {
        self.parts.iter().map(|part| part.allowance.used()).sum()
    }
}

/// A position in a [`Gathered`], for [`Gathered::next_row`]; the default
/// is its first row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RowAt {
    part: usize,
    index: usize,
}

/// A receiver's value.
#[derive(Debug)]
pub enum Received<'a, M: Message> {
    /// `count`: the number of messages received.
    Count(i64),
    /// `sum`, `min`, `max`, `first`, `concat`, `bool_or`, `bool_and`,
    /// `mode`: one message of the messages' type.
    Message(M::View<'a>),
    /// `collect`: the messages received, in collect order.
    Messages(Collected<'a, M>),
}

/// The messages one receiver collected, each with its own boundaries.
#[derive(Debug)]
pub struct Collected<'a, M: Message> {
    messages: &'a M::Storage,
    start: usize,
    end: usize,
}

// Not derived: a derive would require `M` and its storage to be `Clone`.
impl<M: Message> Clone for Received<'_, M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M: Message> Copy for Received<'_, M> {}

impl<M: Message> Clone for Collected<'_, M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M: Message> Copy for Collected<'_, M> {}

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
        (self.start..self.end).filter_map(move |index| M::get(messages, index))
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
    messages: M::Storage,
    allowance: Allowance,
}

impl<M: Combine> Part<M> {
    /// No receivers yet.
    pub(super) fn new(memory: &ComputeMemory) -> Result<Self, ComputeError> {
        Ok(Self {
            receivers: Vec::new(),
            counts: Vec::new(),
            ends: Vec::new(),
            messages: M::Storage::default(),
            allowance: Allowance::unbounded(memory)?,
        })
    }

    /// Appends `message`, charging the capacity it reserves first.
    fn push(&mut self, message: M::View<'_>) -> Result<(), ComputeError> {
        let bytes = M::growth(&self.messages, message);
        self.allowance.take(bytes)?;
        M::push(&mut self.messages, message).map_err(|_| ComputeError::OutOfMemory { bytes })
    }

    /// Releases what the part reserved ahead but did not use.
    pub(super) fn settle(&mut self) {
        self.allowance.settle();
    }

    /// Combines the messages `receiver` received, in collect order, adding
    /// the receiver if there are any; `mode` counts them in `tally`.
    pub(super) fn receive<'a>(
        &mut self,
        combiner: Combiner,
        receiver: DenseId,
        mut received: impl Iterator<Item = M::View<'a>> + Clone,
        tally: &mut Tally<'a, M>,
    ) -> Result<(), ComputeError> {
        let any = match combiner {
            Combiner::Count => {
                let count = received.count();
                if count > 0 {
                    self.allowance.reserve_one(&mut self.counts)?;
                    self.counts.push(count as i64);
                }
                count > 0
            }
            Combiner::Collect => {
                let start = M::len(&self.messages);
                for message in received {
                    self.push(message)?;
                }
                let end = M::len(&self.messages);
                if end > start {
                    self.allowance.reserve_one(&mut self.ends)?;
                    self.ends.push(end);
                }
                end > start
            }
            Combiner::First => match received.next() {
                Some(first) => self.push(first).map(|()| true)?,
                None => false,
            },
            Combiner::Mode => match tally.mode(received)? {
                Some(mode) => self.push(mode).map(|()| true)?,
                None => false,
            },
            Combiner::Concat => {
                let allowance = &mut self.allowance;
                M::concat(&mut self.messages, received, |bytes| allowance.take(bytes))?
            }
            Combiner::Sum
            | Combiner::Min
            | Combiner::Max
            | Combiner::BoolOr
            | Combiner::BoolAnd => {
                let Some(mut value) = received.next() else { return Ok(()) };
                for message in received {
                    value = M::fold(combiner, value, message)?;
                }
                self.push(value).map(|()| true)?
            }
        };
        if any {
            self.allowance.reserve_one(&mut self.receivers)?;
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
            _ => Received::Message(M::get(&self.messages, index)?),
        })
    }
}
