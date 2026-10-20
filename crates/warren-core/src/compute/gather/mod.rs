//! `gather`: fan per-node messages out over the graph's edges and combine
//! them at each receiver, in-engine (DESIGN.md §6.4, tier 2).
//!
//! Messages travel along edges. A receiver gets one copy of each message
//! of each of its *senders*: its distinct neighbours in the gather's
//! direction, over the selected relations. With `direction = in` (the
//! default) the senders of `r` are its in-neighbours `{s : s → r}`, the
//! orientation of `spmv` with `transpose = true`; with `out` they are its
//! out-neighbours `{s : r → s}` (`transpose = false`); with `both` they are
//! the union of the two. An edge in several selected relations, or in both
//! directions, still makes its sender a sender once. Edge weights are not
//! read.
//!
//! A receiver's messages are combined in *collect order*: ascending sender
//! dense id, and a sender's own messages in the order they were pushed.
//! Every receiver is combined on its own, sequentially, so results do not
//! depend on the number of threads.

mod combiner;
mod gathered;
mod messages;
mod neighbourhood;
mod outbox;

use rayon::prelude::*;

pub use combiner::{Combine, Combiner};
pub use gathered::{Collected, Gathered, Received, RowAt};
pub use messages::Messages;

use gathered::Part;
use neighbourhood::{Neighbourhood, Sources};
use outbox::Outbox;

use super::{ComputeError, ComputeMemory, Mask, Workers};
use crate::graph::{DenseId, Direction, GraphCsr, RelationSelection};
use crate::interrupts::{Cancelled, Interrupts};

/// The receivers one task combines; a task checks the cancellation flag
/// first. Small under test, so tests cross task boundaries.
const RECEIVERS_PER_TASK: u64 = if cfg!(test) { 3 } else { 4096 };

/// What to gather.
#[derive(Debug, Clone, Copy)]
pub struct Gather<'a> {
    /// How each receiver's messages are combined.
    pub combiner: Combiner,
    /// Which neighbours of a receiver are its senders.
    pub direction: Direction,
    /// The relations whose edges messages travel along.
    pub relations: RelationSelection<'a>,
    /// Only receivers the mask selects receive messages.
    pub mask: Option<Mask<'a>>,
}

impl Gather<'_> {
    /// Gather with `combiner` from in-neighbours over every relation.
    pub fn new(combiner: Combiner) -> Self {
        Self { combiner, direction: Direction::In, relations: RelationSelection::All, mask: None }
    }
}

/// Fans `messages` out over `graph` and combines them at each receiver.
///
/// In-neighbours are pulled over the reverse CSRs, or, without them,
/// pushed from the senders' forward rows into a per-receiver inbox charged
/// to `memory`, as are the sender index and the result.
pub fn gather<M: Combine, I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &GraphCsr,
    gather: &Gather<'_>,
    messages: &Messages<M>,
) -> Result<Gathered<M>, ComputeError> {
    let combiner = gather.combiner;
    combiner.check(M::KIND)?;
    let id_space = graph.all_relations().forward().id_space();
    let mask = gather.mask;
    if let Some(mask) = mask {
        mask.check(id_space)?;
    }
    let selects = |receiver| mask.is_none_or(|mask: Mask<'_>| mask.selects(receiver));
    let sources = Sources::new(graph, gather.direction, gather.relations)?;
    // Charges on the pool reclaim no held memory (`memory`), so room for
    // the outbox, whose size is known before the fan-out, is reclaimed
    // here, on the caller's thread.
    memory.reclaim_for(Outbox::bytes(messages, id_space));
    workers.run(interrupts, |cancellation| {
        let outbox = Outbox::new(memory, cancellation, messages, id_space)?;
        let neighbourhood =
            Neighbourhood::new(memory, cancellation, sources, &outbox, &selects, id_space)?;
        let parts = (0..id_space.div_ceil(RECEIVERS_PER_TASK))
            .into_par_iter()
            .map(|task| {
                if cancellation.is_cancelled() {
                    return Err(Cancelled.into());
                }
                let mut part = Part::new(memory)?;
                let first = task * RECEIVERS_PER_TASK;
                let receivers = first..id_space.min(first + RECEIVERS_PER_TASK);
                for receiver in receivers.filter(|&receiver| selects(receiver)) {
                    let received =
                        neighbourhood.senders(receiver).flat_map(|sender| outbox.of(sender));
                    part.receive(combiner, receiver as DenseId, received)?;
                }
                part.settle();
                Ok(part)
            })
            .collect::<Result<Vec<_>, ComputeError>>()?;
        Ok(Gathered::new(combiner, parts))
    })
}

// Not under Miri: the compute pool's work stealing runs through
// crossbeam-epoch, which Miri's Stacked Borrows model rejects (a known
// report against crossbeam-epoch 0.9, not this crate, which has no unsafe).
#[cfg(test)]
#[cfg(not(miri))]
mod reference;
#[cfg(test)]
#[cfg(not(miri))]
mod tests;
