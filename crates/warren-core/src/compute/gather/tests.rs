use std::fmt::Debug;
use std::sync::OnceLock;

use super::*;
use crate::compute::reference;
use crate::compute::{
    ArithmeticError, Bytes, ComputeMemory, Message, MessageKind, SemiringElement, Workers,
};
use crate::graph::{DenseId, Direction, GraphCsr};
use crate::interrupts::Uninterrupted;

fn workers() -> &'static Workers {
    static WORKERS: OnceLock<Workers> = OnceLock::new();
    WORKERS.get_or_init(|| Workers::new(3).unwrap())
}

fn memory() -> ComputeMemory {
    ComputeMemory::new(1 << 24)
}

/// A received value, owned so tests can compare it.
#[derive(Debug, Clone, PartialEq)]
enum Owned<T> {
    Count(i64),
    One(T),
    Many(Vec<T>),
}

/// A message type whose views tests can own.
trait Own: Combine {
    type Owned: Debug + Clone + PartialEq;
    fn own(view: Self::View<'_>) -> Self::Owned;
}

impl<E: SemiringElement> Own for E {
    type Owned = E;
    fn own(view: E) -> E {
        view
    }
}

impl Own for Bytes {
    type Owned = Vec<u8>;
    fn own(view: &[u8]) -> Vec<u8> {
        view.to_vec()
    }
}

type Rows<T> = Vec<(DenseId, Owned<T>)>;

fn owned<M: Own>(gathered: &Gathered<M>) -> Rows<M::Owned> {
    gathered
        .iter()
        .map(|(receiver, received)| {
            let value = match received {
                Received::Count(count) => Owned::Count(count),
                Received::Message(message) => Owned::One(M::own(message)),
                Received::Messages(messages) => Owned::Many(messages.iter().map(M::own).collect()),
            };
            (receiver, value)
        })
        .collect()
}

fn messages<'m, M: Message>(
    memory: &ComputeMemory,
    sent: &[(DenseId, M::View<'m>)],
) -> Messages<M> {
    let mut messages = Messages::new(memory).unwrap();
    for &(sender, message) in sent {
        messages.push(sender, message).unwrap();
    }
    messages
}

fn run<'m, M: Own>(
    graph: &GraphCsr,
    gather: &Gather<'_>,
    sent: &[(DenseId, M::View<'m>)],
) -> Result<Rows<M::Owned>, ComputeError> {
    let memory = memory();
    let messages = messages::<M>(&memory, sent);
    let gathered = super::gather(workers(), &memory, &mut Uninterrupted, graph, gather, &messages)?;
    Ok(owned(&gathered))
}

// Relation 0: 0 → 1, 0 → 2, 1 → 2, 2 → 0; relation 1: 0 → 1. Node 3 is
// isolated.
fn example(reverse: bool) -> GraphCsr {
    let edges =
        vec![(0, 0, 1, 2.0), (0, 0, 2, 3.0), (0, 1, 2, 4.0), (0, 2, 0, 5.0), (1, 0, 1, 10.0)];
    reference::graph(4, 2, &edges, true, reverse)
}

/// Node 2 sends two messages; node 3 sends one nobody receives.
const SENT: [(DenseId, f64); 5] = [(2, 3.0), (0, 1.0), (2, 4.0), (1, 2.0), (3, 9.0)];

fn many(values: &[f64]) -> Owned<f64> {
    Owned::Many(values.to_vec())
}

#[test]
fn collect_lists_messages_by_ascending_sender_in_each_direction() {
    for reverse in [true, false] {
        collect_in_each_direction(&example(reverse));
    }
}

fn collect_in_each_direction(graph: &GraphCsr) {
    let collect = |direction| {
        let gather = Gather { direction, ..Gather::new(Combiner::Collect) };
        run::<f64>(graph, &gather, &SENT).unwrap()
    };
    assert_eq!(
        collect(Direction::In),
        [(0, many(&[3.0, 4.0])), (1, many(&[1.0])), (2, many(&[1.0, 2.0]))]
    );
    assert_eq!(
        collect(Direction::Out),
        [(0, many(&[2.0, 3.0, 4.0])), (1, many(&[3.0, 4.0])), (2, many(&[1.0]))]
    );
    assert_eq!(
        collect(Direction::Both),
        [(0, many(&[2.0, 3.0, 4.0])), (1, many(&[1.0, 3.0, 4.0])), (2, many(&[1.0, 2.0]))]
    );
}

fn combined<'m, M: Own>(combiner: Combiner, sent: &[(DenseId, M::View<'m>)]) -> Rows<M::Owned> {
    run::<M>(&example(true), &Gather::new(combiner), sent).unwrap()
}

fn ones<T: Clone>(values: [T; 3]) -> Rows<T> {
    (0..).zip(values.map(Owned::One)).collect()
}

#[test]
fn numeric_combiners_fold_each_receivers_messages() {
    assert_eq!(
        combined::<f64>(Combiner::Count, &SENT),
        (0..).zip([2, 1, 2].map(Owned::Count)).collect::<Vec<_>>()
    );
    assert_eq!(combined::<f64>(Combiner::Sum, &SENT), ones([7.0, 1.0, 3.0]));
    assert_eq!(combined::<f64>(Combiner::Min, &SENT), ones([3.0, 1.0, 1.0]));
    assert_eq!(combined::<f64>(Combiner::Max, &SENT), ones([4.0, 1.0, 2.0]));
    assert_eq!(combined::<f64>(Combiner::First, &SENT), ones([3.0, 1.0, 1.0]));
    let integers = SENT.map(|(sender, value)| (sender, value as i64));
    assert_eq!(combined::<i64>(Combiner::Sum, &integers), ones([7, 1, 3]));
    let reals = SENT.map(|(sender, value)| (sender, value as f32));
    assert_eq!(combined::<f32>(Combiner::Max, &reals), ones([4.0, 1.0, 2.0]));
}

#[test]
fn min_and_max_use_sql_order_and_i64_sums_can_overflow() {
    let sent = [(2, f64::NAN), (2, 1.0)];
    assert_eq!(combined::<f64>(Combiner::Min, &sent), [(0, Owned::One(1.0))]);
    let max = combined::<f64>(Combiner::Max, &sent);
    assert!(matches!(max[..], [(0, Owned::One(nan))] if nan.is_nan()));
    let overflow =
        run::<i64>(&example(true), &Gather::new(Combiner::Sum), &[(2, i64::MAX), (2, 1)]);
    assert_eq!(overflow, Err(ComputeError::Arithmetic(ArithmeticError::BigintOutOfRange)));
}

#[test]
fn boolean_combiners_fold_bool_messages() {
    let sent = [(2, true), (0, false), (2, false), (1, true)];
    assert_eq!(combined::<bool>(Combiner::BoolOr, &sent), ones([true, false, true]));
    assert_eq!(combined::<bool>(Combiner::BoolAnd, &sent), ones([false, false, false]));
    assert_eq!(combined::<bool>(Combiner::First, &sent), ones([true, false, false]));
}

#[test]
fn byte_messages_collect_concatenate_and_count() {
    let sent: [(DenseId, &[u8]); 5] = [(2, b"c"), (0, b"a"), (2, b"d"), (1, b""), (3, b"z")];
    let bytes = |values: [&[&[u8]]; 3]| -> Rows<Vec<u8>> {
        (0..).zip(values.map(|v| Owned::Many(v.iter().map(|b| b.to_vec()).collect()))).collect()
    };
    assert_eq!(
        combined::<Bytes>(Combiner::Collect, &sent),
        bytes([&[b"c", b"d"], &[b"a"], &[b"a", b""]])
    );
    assert_eq!(
        combined::<Bytes>(Combiner::Concat, &sent),
        ones([b"cd".to_vec(), b"a".to_vec(), b"a".to_vec()])
    );
    assert_eq!(
        combined::<Bytes>(Combiner::First, &sent),
        ones([b"c".to_vec(), b"a".to_vec(), b"a".to_vec()])
    );
    assert_eq!(
        combined::<Bytes>(Combiner::Count, &sent),
        (0..).zip([2, 1, 2].map(Owned::Count)).collect::<Vec<_>>()
    );
}

#[test]
fn a_combiner_the_message_kind_does_not_accept_is_an_error() {
    let graph = example(true);
    assert_eq!(
        run::<Bytes>(&graph, &Gather::new(Combiner::Sum), &[]),
        Err(ComputeError::CombinerType { combiner: Combiner::Sum, message: MessageKind::Bytes })
    );
    assert_eq!(
        run::<f64>(&graph, &Gather::new(Combiner::Concat), &SENT),
        Err(ComputeError::CombinerType {
            combiner: Combiner::Concat,
            message: MessageKind::Element(crate::compute::Dtype::F64)
        })
    );
}
