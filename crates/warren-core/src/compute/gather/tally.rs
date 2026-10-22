//! The scratch `mode` counts a receiver's messages in: one per task of a
//! `mode` gather, made when the task first needs it and charged to the
//! compute memory budget while the task runs.

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
    /// The tally in `slot`, made empty there first if there is none,
    /// charging `memory` as it grows.
    pub(super) fn of<'t>(
        slot: &'t mut Option<Self>,
        memory: &ComputeMemory,
    ) -> Result<&'t mut Self, ComputeError> {
        Ok(match slot {
            Some(tally) => tally,
            None => {
                slot.insert(Self { messages: Vec::new(), allowance: Allowance::unbounded(memory)? })
            }
        })
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
