//! The scratch `mode` counts a receiver's messages in: one per task,
//! charged to the compute memory budget while the task runs.

use super::Combine;
use crate::compute::memory::Allowance;
use crate::compute::{ComputeError, ComputeMemory, Message};

/// A receiver's messages, each with its position in collect order.
#[derive(Debug)]
pub(super) struct Tally<'a, M: Message> {
    messages: Vec<(M::View<'a>, usize)>,
    allowance: Allowance,
}

impl<'a, M: Combine> Tally<'a, M> {
    /// An empty tally, charging `memory` as it grows.
    pub(super) fn new(memory: &ComputeMemory) -> Result<Self, ComputeError> {
        Ok(Self { messages: Vec::new(), allowance: Allowance::unbounded(memory)? })
    }

    /// The [`Combiner::Mode`](super::Combiner::Mode) of `received`, in
    /// collect order; `None` if there are none.
    pub(super) fn mode(
        &mut self,
        received: impl Iterator<Item = M::View<'a>>,
    ) -> Result<Option<M::View<'a>>, ComputeError> {
        self.messages.clear();
        for (position, message) in received.enumerate() {
            self.allowance.reserve_one(&mut self.messages)?;
            self.messages.push((message, position));
        }
        M::mode(&mut self.messages)
    }
}
