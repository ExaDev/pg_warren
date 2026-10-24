//! A receiver's senders: its distinct neighbours in the gather's direction
//! over the selected relations, in ascending dense-id order; per relation,
//! a sender once in each relation linking it.
//!
//! CSR rows are read where the graph stores them (pull): the forward CSR
//! for out-neighbours, the reverse CSR for in-neighbours. Without a reverse
//! CSR, in-neighbours come from an inbox scattered over the forward CSRs
//! from the senders that have messages (push).

use std::iter::{Copied, Peekable};
use std::mem::size_of;
use std::slice::Iter;

use super::outbox::{check, running_ends, Outbox};
use super::Messages;
use crate::compute::{ComputeError, ComputeMemory, MemoryCharge, Message};
use crate::graph::{
    AnyCsr, CsrRows, DenseId, Direction, GraphCsr, Neighbours, RelationCsr, RelationSelection,
    RowNeighbours,
};
use crate::interrupts::Cancellation;

/// The CSRs a gather reads, in groups: a sender in several rows of one
/// group is a sender once in it. One group for the whole selection, or per
/// relation one for each relation.
#[derive(Debug)]
pub(super) struct Sources<'g> {
    groups: Vec<Group<'g>>,
}

/// One group's CSRs: rows pulled as they are, and forward rows to push
/// into an inbox.
#[derive(Debug)]
struct Group<'g> {
    pull: Vec<CsrRows<'g>>,
    push: Vec<&'g AnyCsr>,
}

impl<'g> Group<'g> {
    /// What gives each receiver its neighbours in `direction`: the forward
    /// rows, the reverse rows if there are any, and otherwise `forward` to
    /// push.
    fn new(
        direction: Direction,
        forward_rows: Vec<CsrRows<'g>>,
        reverse_rows: Option<Vec<CsrRows<'g>>>,
        forward: Vec<&'g AnyCsr>,
    ) -> Self {
        let (mut pull, mut push) = (Vec::new(), Vec::new());
        if direction != Direction::In {
            pull.extend(forward_rows);
        }
        if direction != Direction::Out {
            match reverse_rows {
                Some(reverse) => pull.extend(reverse),
                None => push.extend(forward),
            }
        }
        Self { pull, push }
    }

    /// One relation's CSRs.
    fn of(direction: Direction, relation: &'g RelationCsr) -> Self {
        let reverse = relation.reverse().map(|reverse| vec![CsrRows::from(reverse)]);
        let forward = relation.forward();
        Self::new(direction, vec![CsrRows::from(forward)], reverse, vec![forward])
    }
}

impl<'g> Sources<'g> {
    /// The CSRs of `relations` that give each receiver its neighbours in
    /// `direction`, in one group, or `per_relation` in one per relation.
    pub(super) fn new(
        graph: &'g GraphCsr,
        direction: Direction,
        relations: RelationSelection<'_>,
        per_relation: bool,
    ) -> Result<Self, ComputeError> {
        let selected = graph
            .selected(relations)
            .map_err(|relation| ComputeError::UnknownRelation { relation: relation.index() })?;
        if per_relation {
            // Every relation's own CSRs, not the merged CSR `selected` gives.
            let each: Vec<&RelationCsr> = match relations {
                RelationSelection::All => graph.relations().iter().collect(),
                RelationSelection::Only(_) => selected,
            };
            let groups = each.into_iter().map(|relation| Group::of(direction, relation)).collect();
            return Ok(Self { groups });
        }
        let forward: Vec<&AnyCsr> = selected.iter().map(|relation| relation.forward()).collect();
        // Every relation is read through its all-relations rows, stored or
        // merged on the fly; a selection through each relation's CSRs.
        let (forward_rows, reverse_rows): (Vec<CsrRows<'g>>, Option<Vec<CsrRows<'g>>>) =
            match relations {
                RelationSelection::All => {
                    let all = graph.all_relations();
                    (vec![all.forward()], all.reverse().map(|reverse| vec![reverse]))
                }
                RelationSelection::Only(_) => (
                    forward.iter().copied().map(CsrRows::from).collect(),
                    selected.iter().map(|r| r.reverse().map(CsrRows::from)).collect(),
                ),
            };
        Ok(Self { groups: vec![Group::new(direction, forward_rows, reverse_rows, forward)] })
    }

    /// Whether any group pushes into an inbox.
    fn pushes(&self) -> bool {
        self.groups.iter().any(|group| !group.push.is_empty())
    }

    /// Whether an inbox must tag each sender with its group.
    fn tagged(&self) -> bool {
        self.groups.len() > 1
    }
}

/// The senders of every receiver.
#[derive(Debug)]
pub(super) struct Neighbourhood<'g> {
    groups: Vec<Vec<CsrRows<'g>>>,
    inbox: Option<Inbox>,
}

impl<'g> Neighbourhood<'g> {
    /// At most the bytes [`Self::new`] charges for the inbox, if `sources`
    /// push: its offsets, and a sender (and with several groups its group)
    /// for each pushed edge of a sender with messages (those senders'
    /// degrees, summed with repeats, or every edge pushed, whichever is
    /// fewer).
    pub(super) fn bytes<M: Message>(
        sources: &Sources<'_>,
        messages: &Messages<M>,
        id_space: u64,
    ) -> u64 {
        if !sources.pushes() {
            return 0;
        }
        let push = || sources.groups.iter().flat_map(|group| group.push.iter());
        let edges = push().map(|csr| csr.edge_count()).fold(0, u64::saturating_add);
        let sent = messages
            .senders()
            .iter()
            .map(|&sender| push().map(|csr| csr.degree(sender)).fold(0, u64::saturating_add));
        let pairs = sent.fold(0, u64::saturating_add).min(edges);
        let offsets = id_space.saturating_add(1).saturating_mul(size_of::<u64>() as u64);
        let tag = if sources.tagged() { size_of::<Tag>() } else { 0 };
        let entry = (size_of::<DenseId>() + tag) as u64;
        offsets.saturating_add(pairs.saturating_mul(entry))
    }

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
        let inbox = match sources.pushes() {
            false => None,
            true => {
                let groups: Vec<&[&AnyCsr]> =
                    sources.groups.iter().map(|group| group.push.as_slice()).collect();
                let push = Push { groups: &groups, outbox, selects };
                Some(Inbox::new(memory, cancellation, push, id_space)?)
            }
        };
        let groups = sources.groups.into_iter().map(|group| group.pull).collect();
        Ok(Self { groups, inbox })
    }

    /// `receiver`'s senders, ascending: each once, or with several groups
    /// once in each group that holds it.
    pub(super) fn senders(&self, receiver: DenseId) -> impl Iterator<Item = DenseId> + Clone + '_ {
        match self.groups.as_slice() {
            [rows] => Repeated::Once(self.group(receiver, 0, rows)),
            groups => Repeated::Each(EachGroup::new(
                groups.iter().enumerate().map(|(tag, rows)| self.group(receiver, tag, rows)),
            )),
        }
    }

    /// `receiver`'s senders in group `tag`, which pulls `rows`.
    fn group<'s>(&'s self, receiver: DenseId, tag: usize, rows: &'s [CsrRows<'g>]) -> Senders<'s> {
        let rows = rows.iter().map(move |rows| Row::Merged(rows.neighbours(receiver)));
        let inbox = self.inbox.iter().map(move |inbox| Row::Inbox(inbox.row(receiver, tag)));
        Senders::new(rows.chain(inbox))
    }
}

/// A group's position, as an inbox tags its senders.
type Tag = u32;

/// What an inbox is scattered from: each group's forward CSRs, the senders
/// with messages, and the receivers selected.
struct Push<'p, M: Message, S> {
    groups: &'p [&'p [&'p AnyCsr]],
    outbox: &'p Outbox<M>,
    selects: &'p S,
}

impl<M: Message, S: Fn(DenseId) -> bool> Push<'_, M, S> {
    /// The selected receivers of `sender`'s messages, if it has any, each
    /// with the group it receives them through.
    fn receivers(&self, sender: DenseId) -> impl Iterator<Item = (DenseId, Tag)> + '_ {
        let sends = self.outbox.sends(sender);
        let groups = self.groups.iter().zip(0..).filter(move |_| sends);
        groups.flat_map(move |(forward, tag)| {
            let rows = forward.iter().map(move |csr| Row::Csr(csr.neighbours(sender)));
            let receivers = Senders::new(rows).filter(|&receiver| (self.selects)(receiver));
            receivers.map(move |receiver| (receiver, tag))
        })
    }
}

/// Each selected receiver's in-neighbours that have messages, scattered
/// from the forward CSRs: a stable counting sort of `(receiver, sender)`
/// pairs, so each row lists its senders in ascending order (within each
/// group, with several groups).
#[derive(Debug)]
struct Inbox {
    /// `id_space + 1` positions into `senders`.
    offsets: Vec<u64>,
    senders: Vec<DenseId>,
    /// Each sender's group, with several groups.
    tags: Option<Vec<Tag>>,
    _charges: [Option<MemoryCharge>; 3],
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
            for (receiver, _) in push.receivers(sender) {
                offsets[receiver as usize] += 1;
            }
        }
        // As in the outbox: running ends, then fill last to first.
        running_ends(&mut offsets);
        let pairs = offsets[id_space as usize] as usize;
        let (mut senders, senders_charge) = memory.filled(pairs, 0 as DenseId)?;
        let (mut tags, tags_charge) = match push.groups.len() > 1 {
            true => {
                memory.filled(pairs, 0 as Tag).map(|(tags, charge)| (Some(tags), Some(charge)))?
            }
            false => (None, None),
        };
        for sender in (0..id_space).rev() {
            check(cancellation, sender as usize)?;
            for (receiver, tag) in push.receivers(sender) {
                let offset = &mut offsets[receiver as usize];
                *offset -= 1;
                senders[*offset as usize] = sender;
                if let Some(tags) = &mut tags {
                    tags[*offset as usize] = tag;
                }
            }
        }
        let _charges = [Some(offsets_charge), Some(senders_charge), tags_charge];
        Ok(Self { offsets, senders, tags, _charges })
    }

    /// `receiver`'s senders in group `tag`.
    fn row(&self, receiver: DenseId, tag: usize) -> InboxRow<'_> {
        let receiver = receiver as usize;
        let row = self.offsets[receiver] as usize..self.offsets[receiver + 1] as usize;
        let tags = self.tags.as_ref().map(|tags| tags[row.clone()].iter());
        InboxRow { senders: self.senders[row].iter().copied(), tags, tag }
    }
}

/// An inbox row's senders in one group.
#[derive(Debug, Clone)]
struct InboxRow<'a> {
    senders: Copied<Iter<'a, DenseId>>,
    tags: Option<Iter<'a, Tag>>,
    tag: usize,
}

impl Iterator for InboxRow<'_> {
    type Item = DenseId;

    fn next(&mut self) -> Option<DenseId> {
        loop {
            let sender = self.senders.next()?;
            let Some(tags) = &mut self.tags else {
                return Some(sender);
            };
            if tags.next().is_some_and(|&tag| tag as usize == self.tag) {
                return Some(sender);
            }
        }
    }
}

/// A receiver's senders: one group's, or each group's merged, a sender
/// once for each group that holds it.
#[derive(Debug, Clone)]
enum Repeated<'a> {
    Once(Senders<'a>),
    Each(EachGroup<'a>),
}

impl Iterator for Repeated<'_> {
    type Item = DenseId;

    fn next(&mut self) -> Option<DenseId> {
        match self {
            Self::Once(senders) => senders.next(),
            Self::Each(senders) => senders.next(),
        }
    }
}

/// The groups' senders merged in ascending order, a sender repeated once
/// for each group that holds it.
#[derive(Debug, Clone)]
struct EachGroup<'a> {
    groups: Vec<Peekable<Senders<'a>>>,
    sender: DenseId,
    repeats: usize,
}

impl<'a> EachGroup<'a> {
    fn new(groups: impl Iterator<Item = Senders<'a>>) -> Self {
        Self { groups: groups.map(Iterator::peekable).collect(), sender: 0, repeats: 0 }
    }
}

impl Iterator for EachGroup<'_> {
    type Item = DenseId;

    fn next(&mut self) -> Option<DenseId> {
        if self.repeats == 0 {
            let smallest =
                self.groups.iter_mut().filter_map(|group| group.peek().copied()).min()?;
            let holding = self.groups.iter_mut().filter_map(|group| group.next_if_eq(&smallest));
            self.repeats = holding.count();
            self.sender = smallest;
        }
        self.repeats -= 1;
        Some(self.sender)
    }
}

/// One sorted row of senders.
#[derive(Debug, Clone)]
enum Row<'a> {
    Csr(Neighbours<'a>),
    Merged(RowNeighbours<'a>),
    Inbox(InboxRow<'a>),
}

impl Iterator for Row<'_> {
    type Item = DenseId;

    fn next(&mut self) -> Option<DenseId> {
        match self {
            Self::Csr(row) => row.next(),
            Self::Merged(row) => row.next(),
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
