//! A receiver's senders: its distinct neighbours in the gather's direction
//! over the selected relations, in ascending dense-id order.
//!
//! CSR rows are read where the graph stores them (pull): the forward CSR
//! for out-neighbours, the reverse CSR for in-neighbours. Without a reverse
//! CSR, in-neighbours come from an inbox scattered over the forward CSRs
//! from the senders that have messages (push).

use std::iter::{Copied, Peekable};
use std::slice::Iter;

use super::outbox::{check, running_ends, Outbox};
use crate::compute::{ComputeError, ComputeMemory, MemoryCharge, Message};
use crate::graph::{AnyCsr, DenseId, Direction, GraphCsr, Neighbours, RelationSelection};
use crate::interrupts::Cancellation;

/// The CSRs a gather reads: rows pulled as they are, and forward rows to
/// push into an inbox.
#[derive(Debug)]
pub(super) struct Sources<'g> {
    pull: Vec<&'g AnyCsr>,
    push: Vec<&'g AnyCsr>,
}

impl<'g> Sources<'g> {
    /// The CSRs of `relations` that give each receiver its neighbours in
    /// `direction`.
    pub(super) fn new(
        graph: &'g GraphCsr,
        direction: Direction,
        relations: RelationSelection<'_>,
    ) -> Result<Self, ComputeError> {
        let selected = graph
            .selected(relations)
            .map_err(|relation| ComputeError::UnknownRelation { relation: relation.index() })?;
        let forward = selected.iter().map(|relation| relation.forward());
        let reverse: Option<Vec<&AnyCsr>> = selected.iter().map(|r| r.reverse()).collect();
        let (mut pull, mut push) = (Vec::new(), Vec::new());
        if direction != Direction::In {
            pull.extend(forward.clone());
        }
        if direction != Direction::Out {
            match reverse {
                Some(reverse) => pull.extend(reverse),
                None => push.extend(forward),
            }
        }
        Ok(Self { pull, push })
    }
}

/// The senders of every receiver.
#[derive(Debug)]
pub(super) struct Neighbourhood<'g> {
    rows: Vec<&'g AnyCsr>,
    inbox: Option<Inbox>,
}

impl<'g> Neighbourhood<'g> {
    /// The neighbourhood `sources` read, scattering the inbox for the
    /// receivers `selects` selects if there are rows to push.
    pub(super) fn new<M: Message>(
        memory: &ComputeMemory,
        cancellation: &Cancellation,
        sources: Sources<'g>,
        outbox: &Outbox<M>,
        selects: &impl Fn(DenseId) -> bool,
        id_space: u64,
    ) -> Result<Self, ComputeError> {
        let inbox = match sources.push.is_empty() {
            true => None,
            false => {
                let push = Push { forward: &sources.push, outbox, selects };
                Some(Inbox::new(memory, cancellation, push, id_space)?)
            }
        };
        Ok(Self { rows: sources.pull, inbox })
    }

    /// `receiver`'s senders, ascending and distinct.
    pub(super) fn senders(&self, receiver: DenseId) -> impl Iterator<Item = DenseId> + Clone + '_ {
        let rows = self.rows.iter().map(|csr| Row::Csr(csr.neighbours(receiver)));
        let inbox = self.inbox.iter().map(|inbox| Row::Inbox(inbox.row(receiver).iter().copied()));
        Senders::new(rows.chain(inbox))
    }
}

/// What an inbox is scattered from: the forward CSRs, the senders with
/// messages, and the receivers selected.
struct Push<'p, M: Message, S> {
    forward: &'p [&'p AnyCsr],
    outbox: &'p Outbox<M>,
    selects: &'p S,
}

impl<M: Message, S: Fn(DenseId) -> bool> Push<'_, M, S> {
    /// The selected receivers of `sender`'s messages, if it has any.
    fn receivers(&self, sender: DenseId) -> impl Iterator<Item = DenseId> + '_ {
        let sends = self.outbox.sends(sender);
        let rows = self.forward.iter().filter(move |_| sends);
        let receivers = Senders::new(rows.map(move |csr| Row::Csr(csr.neighbours(sender))));
        receivers.filter(|&receiver| (self.selects)(receiver))
    }
}

/// Each selected receiver's in-neighbours that have messages, scattered
/// from the forward CSRs: a stable counting sort of `(receiver, sender)` pairs, so
/// each row lists its senders in ascending order.
#[derive(Debug)]
struct Inbox {
    /// `id_space + 1` positions into `senders`.
    offsets: Vec<u64>,
    senders: Vec<DenseId>,
    _charges: [MemoryCharge; 2],
}

impl Inbox {
    fn new(
        memory: &ComputeMemory,
        cancellation: &Cancellation,
        push: Push<'_, impl Message, impl Fn(DenseId) -> bool>,
        id_space: u64,
    ) -> Result<Self, ComputeError> {
        let (mut offsets, offsets_charge) = memory.filled(id_space as usize + 1, 0u64)?;
        for sender in 0..id_space {
            check(cancellation, sender as usize)?;
            for receiver in push.receivers(sender) {
                offsets[receiver as usize] += 1;
            }
        }
        // As in the outbox: running ends, then fill last to first.
        running_ends(&mut offsets);
        let pairs = offsets[id_space as usize] as usize;
        let (mut senders, senders_charge) = memory.filled(pairs, 0 as DenseId)?;
        for sender in (0..id_space).rev() {
            check(cancellation, sender as usize)?;
            for receiver in push.receivers(sender) {
                let offset = &mut offsets[receiver as usize];
                *offset -= 1;
                senders[*offset as usize] = sender;
            }
        }
        Ok(Self { offsets, senders, _charges: [offsets_charge, senders_charge] })
    }

    fn row(&self, receiver: DenseId) -> &[DenseId] {
        let receiver = receiver as usize;
        &self.senders[self.offsets[receiver] as usize..self.offsets[receiver + 1] as usize]
    }
}

/// One sorted row of senders.
#[derive(Debug, Clone)]
enum Row<'a> {
    Csr(Neighbours<'a>),
    Inbox(Copied<Iter<'a, DenseId>>),
}

impl Iterator for Row<'_> {
    type Item = DenseId;

    fn next(&mut self) -> Option<DenseId> {
        match self {
            Self::Csr(row) => row.next(),
            Self::Inbox(row) => row.next(),
        }
    }
}

/// The distinct senders of sorted rows, ascending: one row as it is, or a
/// k-way merge of several.
#[derive(Debug, Clone)]
enum Senders<'a> {
    One(Row<'a>),
    Many(Vec<Peekable<Row<'a>>>),
}

impl<'a> Senders<'a> {
    fn new(rows: impl Iterator<Item = Row<'a>>) -> Self {
        let mut rows = rows.into_iter();
        match (rows.next(), rows.next()) {
            (Some(row), None) => Self::One(row),
            (first, second) => {
                let rows = first.into_iter().chain(second).chain(rows);
                Self::Many(rows.map(Iterator::peekable).collect())
            }
        }
    }
}

impl Iterator for Senders<'_> {
    type Item = DenseId;

    fn next(&mut self) -> Option<DenseId> {
        match self {
            Self::One(row) => row.next(),
            Self::Many(rows) => {
                let smallest = rows.iter_mut().filter_map(|row| row.peek().copied()).min()?;
                for row in rows {
                    row.next_if_eq(&smallest);
                }
                Some(smallest)
            }
        }
    }
}
