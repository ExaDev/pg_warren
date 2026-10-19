//! What a spectral kernel runs with: the compute threads, the compute
//! memory budget and the caller's interrupts (DESIGN.md §6.9).

use crate::compute::{ComputeError, ComputeMemory, Workers};
use crate::interrupts::{Cancellation, Interrupts};

/// The compute threads, memory budget and interrupts of one call.
#[derive(Debug)]
pub struct Resources<'r, I> {
    /// The compute threads.
    pub workers: &'r Workers,
    /// The budget every buffer is charged to.
    pub memory: &'r ComputeMemory,
    /// Polled between kernel calls and while a kernel runs.
    pub interrupts: &'r mut I,
}

impl<I: Interrupts> Resources<'_, I> {
    /// Runs `job` on the compute threads while polling the interrupts.
    pub(super) fn run<R: Send>(
        &mut self,
        job: impl FnOnce(&Cancellation) -> Result<R, ComputeError> + Send,
    ) -> Result<R, ComputeError> {
        self.workers.run(self.interrupts, job)
    }
}
