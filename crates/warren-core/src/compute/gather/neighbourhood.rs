//! A receiver's senders: its distinct neighbours in the gather's direction
//! over the selected relations, in ascending dense-id order.
//!
//! CSR rows are read where the graph stores them (pull): the forward CSR
//! for out-neighbours, the reverse CSR for in-neighbours. Without a reverse
//! CSR, in-neighbours come from an inbox scattered over the forward CSRs
//! from the senders that have messages (push).

use std::iter::{Copied, Peekable};
use std::slice::Iter;

use super::outbox::{check, Outbox};
use crate::compute::{ComputeError, ComputeMemory, MemoryCharge};
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
    /// The neighbourhood `sources` read, scattering the inbox if needed.
    pub(super) fn new(
        memory: &ComputeMemory,
        cancellation: &Cancellation,
        sources: Sources<'g>,
        outbox: &Outbox,
        id_space: u64,
    ) -> Result<Self, ComputeError> {
        let inbox = match sources.push.is_empty() {
            true => None,
            false => Some(Inbox::new(memory, cancellation, &sources.push, outbox, id_space)?),
        };
        Ok(Self { rows: sources.pull, inbox })
    }

    /// `receiver`'s senders, ascending and distinct.
    pub(super) fn senders(&self, receiver: DenseId) -> impl Iterator<Item = DenseId> + '_ {
        let rows = self.rows.iter().map(|csr| Row::Csr(csr.neighbours(receiver)));
        let inbox = self.inbox.iter().map(|inbox| Row::Inbox(inbox.row(receiver).iter().copied()));
        Senders::new(rows.chain(inbox))
    }
}

/// Each receiver's in-neighbours that have messages, scattered from the
/// forward CSRs: a stable counting sort of `(receiver, sender)` pairs, so
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
        forward: &[&AnyCsr],
        outbox: &Outbox,
        id_space: u64,
    ) -> Result<Self, ComputeError> {
        let (mut offsets, offsets_charge) = memory.filled(id_space as usize + 1, 0u64)?;
        let receivers = |sender: DenseId| {
            let sends = !outbox.of(sender).is_empty();
            let rows = forward.iter().filter(move |_| sends);
            Senders::new(rows.map(move |csr| Row::Csr(csr.neighbours(sender))))
        };
        for sender in 0..id_space {
            check(cancellation, sender as usize)?;
            for receiver in receivers(sender) {
                offsets[receiver as usize] += 1;
            }
        }
        // As in the outbox: running ends, then fill last to first.
        let mut end = 0;
        for offset in &mut offsets[..id_space as usize] {
            end += *offset;
            *offset = end;
        }
        offsets[id_space as usize] = end;
        let (mut senders, senders_charge) = memory.filled(end as usize, 0 as DenseId)?;
        for sender in (0..id_space).rev() {
            check(cancellation, sender as usize)?;
            for receiver in receivers(sender) {
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
