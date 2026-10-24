//! A naive `gather` for the property tests: join the distinct
//! `(sender, receiver)` pairs of the selected edges (or, per relation, the
//! distinct `(relation, sender, receiver)` triples) with the messages, and
//! group by receiver in collect order.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt::Debug;

use super::{Combine, Combiner};
use crate::compute::reference::Edges;
use crate::compute::{Bytes, ComputeError, Element, SemiringElement};
use crate::graph::{DenseId, Direction};

/// A received value, owned so tests can compare it.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Owned<T> {
    Count(i64),
    One(T),
    Many(Vec<T>),
}

/// Each receiver's value, in ascending dense-id order.
pub(super) type Rows<T> = Vec<(DenseId, Owned<T>)>;

/// A message type whose views tests can own.
pub(super) trait Own: Combine {
    type Owned: Debug + Clone + PartialEq;
    fn own(view: Self::View<'_>) -> Self::Owned;
    fn view(owned: &Self::Owned) -> Self::View<'_>;
    /// Equality, with floats in SQL order (`NaN` equals `NaN`).
    fn same(a: &Self::Owned, b: &Self::Owned) -> bool;
    /// SQL order.
    fn order(a: &Self::Owned, b: &Self::Owned) -> Ordering;
    /// The concatenation of `parts`, for message types `concat` accepts.
    fn joined(parts: &[Self::Owned]) -> Option<Self::Owned>;
}

impl<E: SemiringElement> Own for E {
    type Owned = E;
    fn own(view: E) -> E {
        view
    }
    fn view(owned: &E) -> E {
        *owned
    }
    fn same(a: &E, b: &E) -> bool {
        a.sql_cmp(*b).is_eq()
    }
    fn order(a: &E, b: &E) -> Ordering {
        a.sql_cmp(*b)
    }
    fn joined(_: &[E]) -> Option<E> {
        None
    }
}

impl Own for Bytes {
    type Owned = Vec<u8>;
    fn own(view: &[u8]) -> Vec<u8> {
        view.to_vec()
    }
    fn view(owned: &Vec<u8>) -> &[u8] {
        owned
    }
    fn same(a: &Vec<u8>, b: &Vec<u8>) -> bool {
        a == b
    }
    fn order(a: &Vec<u8>, b: &Vec<u8>) -> Ordering {
        a.cmp(b)
    }
    fn joined(parts: &[Vec<u8>]) -> Option<Vec<u8>> {
        Some(parts.concat())
    }
}

/// Whether two results agree, values compared with [`Own::same`].
pub(super) fn same_rows<M: Own>(a: &Rows<M::Owned>, b: &Rows<M::Owned>) -> bool {
    let same = |a: &Owned<M::Owned>, b: &Owned<M::Owned>| match (a, b) {
        (Owned::Count(a), Owned::Count(b)) => a == b,
        (Owned::One(a), Owned::One(b)) => M::same(a, b),
        (Owned::Many(a), Owned::Many(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| M::same(a, b))
        }
        _ => false,
    };
    a.len() == b.len() && a.iter().zip(b).all(|((r, a), (s, b))| r == s && same(a, b))
}

/// What a gather is over, spelled out.
pub(super) struct Naive<'a, T> {
    pub(super) id_space: u64,
    pub(super) edges: &'a Edges,
    pub(super) selected: Option<&'a [usize]>,
    pub(super) direction: Direction,
    /// Whether each receiver is selected.
    pub(super) selects: Vec<bool>,
    /// Whether a receiver's own messages come first.
    pub(super) include_self: bool,
    /// Whether a sender sends once in each relation linking it.
    pub(super) per_relation: bool,
    pub(super) sent: &'a [(DenseId, T)],
}

/// The naive gather of `naive` under `combiner`.
pub(super) fn gather<M: Own>(
    naive: &Naive<'_, M::Owned>,
    combiner: Combiner,
) -> Result<Rows<M::Owned>, ComputeError> {
    combiner.check(M::KIND)?;
    if let Some(&(sender, _)) = naive.sent.iter().find(|(sender, _)| *sender >= naive.id_space) {
        return Err(ComputeError::SenderOutsideIdSpace { sender, id_space: naive.id_space });
    }
    // (receiver, sender, relation), the relation 0 unless per relation.
    let mut links = BTreeSet::new();
    for &(relation, source, target, _) in naive.edges {
        if naive.selected.is_some_and(|selected| !selected.contains(&relation)) {
            continue;
        }
        let relation = if naive.per_relation { relation } else { 0 };
        if naive.direction != Direction::Out {
            links.insert((target, source, relation));
        }
        if naive.direction != Direction::In {
            links.insert((source, target, relation));
        }
    }
    let mut rows = Vec::new();
    for receiver in (0..naive.id_space).filter(|&r| naive.selects[r as usize]) {
        let own = naive.include_self.then_some(receiver);
        let senders = links
            .range((receiver, 0, 0)..=(receiver, DenseId::MAX, usize::MAX))
            .map(|&(_, s, _)| s);
        let received: Vec<M::Owned> = own
            .into_iter()
            .chain(senders)
            .flat_map(|sender| naive.sent.iter().filter(move |(s, _)| *s == sender))
            .map(|(_, message)| message.clone())
            .collect();
        if !received.is_empty() {
            rows.push((receiver, combine::<M>(combiner, received)?));
        }
    }
    Ok(rows)
}

fn combine<M: Own>(
    combiner: Combiner,
    received: Vec<M::Owned>,
) -> Result<Owned<M::Owned>, ComputeError> {
    Ok(match combiner {
        Combiner::Collect => Owned::Many(received),
        Combiner::Count => Owned::Count(received.len() as i64),
        Combiner::First => Owned::One(received[0].clone()),
        Combiner::Concat => Owned::One(M::joined(&received).unwrap()),
        Combiner::Mode => {
            // The most votes, then the smallest, then the first received.
            let votes = |m: &M::Owned| received.iter().filter(|r| M::same(r, m)).count();
            let wins =
                |a: &&M::Owned, b: &&M::Owned| votes(b).cmp(&votes(a)).then_with(|| M::order(a, b));
            Owned::One(received.iter().min_by(wins).unwrap().clone())
        }
        _ => {
            let mut value = received[0].clone();
            for message in &received[1..] {
                let folded = M::fold(combiner, M::view(&value), M::view(message)).map(M::own);
                value = folded?;
            }
            Owned::One(value)
        }
    })
}

/// Whether a mask value selects its receiver.
pub(super) fn selects(value: f64, complement: bool) -> bool {
    value.is_nonzero() != complement
}
