use std::fmt::Debug;
use std::sync::OnceLock;

use proptest::prelude::*;

use super::reference::{self as naive, Naive, Own, Owned, Rows};
use super::*;
use crate::compute::reference::{self, Edges};
use crate::compute::{
    AnyVector, ArithmeticError, Bytes, ComputeMemory, Element, Mask, Message, MessageKind, Vector,
    Workers,
};
use crate::graph::{CsrLayout, DenseId, Direction, EdgeList, GraphCsr, RelationIndex};
use crate::interrupts::{Cancelled, Uninterrupted};

fn workers() -> &'static Workers {
    static WORKERS: OnceLock<Workers> = OnceLock::new();
    WORKERS.get_or_init(|| Workers::new(3).unwrap())
}

fn memory() -> ComputeMemory {
    ComputeMemory::new(1 << 24)
}

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

fn vector<E: Element>(values: Vec<E>) -> AnyVector
where
    AnyVector: From<Vector<E>>,
{
    Vector::from_values(&memory(), values.len() as u64, 1, values).unwrap().into()
}

#[test]
fn a_mask_restricts_the_receivers() {
    let mask = vector(vec![1i64, 0, -7, 0]);
    for reverse in [true, false] {
        let graph = example(reverse);
        let masked = |complement| {
            let gather = Gather {
                mask: Some(Mask { vector: &mask, complement }),
                ..Gather::new(Combiner::Collect)
            };
            run::<f64>(&graph, &gather, &SENT).unwrap()
        };
        assert_eq!(masked(false), [(0, many(&[3.0, 4.0])), (2, many(&[1.0, 2.0]))]);
        assert_eq!(masked(true), [(1, many(&[1.0]))]);
    }
}

#[test]
fn relations_select_the_edges_messages_travel_along() {
    let r = reference::relations(3);
    for reverse in [true, false] {
        let graph = example(reverse);
        let only = |relations: &[RelationIndex], direction| {
            let gather = Gather {
                direction,
                relations: RelationSelection::Only(relations),
                ..Gather::new(Combiner::Collect)
            };
            run::<f64>(&graph, &gather, &SENT)
        };
        assert_eq!(only(&r[1..2], Direction::In), Ok(vec![(1, many(&[1.0]))]));
        assert_eq!(only(&r[1..2], Direction::Both), Ok(vec![(0, many(&[2.0])), (1, many(&[1.0]))]));
        assert_eq!(only(&[r[1], r[0], r[1]], Direction::In), only(&r[..2], Direction::In));
        assert_eq!(only(&[], Direction::Both), Ok(vec![]));
        assert_eq!(
            only(&r[2..], Direction::In),
            Err(ComputeError::UnknownRelation { relation: 2 })
        );
    }
}

#[test]
fn senders_and_masks_must_fit_the_graph() {
    let graph = example(true);
    let collect = Gather::new(Combiner::Collect);
    assert_eq!(
        run::<f64>(&graph, &collect, &[(0, 1.0), (4, 2.0)]),
        Err(ComputeError::SenderOutsideIdSpace { sender: 4, id_space: 4 })
    );
    let short = vector(vec![1.0; 3]);
    let gather = Gather { mask: Some(Mask { vector: &short, complement: false }), ..collect };
    assert_eq!(
        run::<f64>(&graph, &gather, &SENT),
        Err(ComputeError::IdSpaceMismatch { expected: 4, found: 3 })
    );
    let wide = Vector::from_values(&memory(), 4, 2, vec![1.0; 8]).unwrap().into();
    let gather = Gather { mask: Some(Mask { vector: &wide, complement: false }), ..collect };
    assert_eq!(
        run::<f64>(&graph, &gather, &SENT),
        Err(ComputeError::WidthMismatch { expected: 1, found: 2 })
    );
}

#[test]
fn the_result_is_charged_and_the_scratch_released() {
    for reverse in [true, false] {
        let graph = example(reverse);
        let memory = memory();
        let messages = messages::<f64>(&memory, &SENT);
        let gather = Gather { direction: Direction::Both, ..Gather::new(Combiner::Collect) };
        let gathered =
            super::gather(workers(), &memory, &mut Uninterrupted, &graph, &gather, &messages)
                .unwrap();
        // The reserved capacity: four receivers and ends (for three), and
        // eight collected f64s.
        assert_eq!(gathered.bytes(), 4 * 8 + 4 * 8 + 8 * 8);
        assert_eq!(memory.in_use(), messages.bytes() + gathered.bytes());
        assert_eq!((gathered.len(), gathered.combiner()), (3, Combiner::Collect));
    }
}

#[test]
fn a_gather_past_the_budget_is_an_error_and_charges_nothing() {
    for reverse in [true, false] {
        let graph = example(reverse);
        let budget = ComputeMemory::new(160);
        let messages = messages::<f64>(&budget, &SENT[..2]);
        let gather = Gather { direction: Direction::Both, ..Gather::new(Combiner::Collect) };
        let result =
            super::gather(workers(), &budget, &mut Uninterrupted, &graph, &gather, &messages);
        assert!(matches!(result, Err(ComputeError::MemoryLimit { .. })));
        assert_eq!(budget.in_use(), messages.bytes());
    }
}

/// A reclaim hook that frees all of what it holds whatever it is asked.
#[derive(Debug)]
struct FreesAll(u64);

impl crate::compute::Reclaim for FreesAll {
    fn reclaim(&self, _bytes: u64) -> u64 {
        self.0
    }
}

#[test]
fn held_memory_is_reclaimed_before_the_fan_out() {
    for reverse in [true, false] {
        let graph = example(reverse);
        let sizing = memory();
        let bytes = messages::<f64>(&sizing, &SENT).bytes();
        // No room past the messages and 1 MiB held: the outbox and the
        // result are charged on the pool, where nothing is reclaimed.
        let held = 1 << 20;
        let budget = ComputeMemory::reclaiming(bytes + held, held, held, Box::new(FreesAll(held)));
        let messages = messages::<f64>(&budget, &SENT);
        let gather = Gather { direction: Direction::Both, ..Gather::new(Combiner::Collect) };
        let gathered =
            super::gather(workers(), &budget, &mut Uninterrupted, &graph, &gather, &messages)
                .unwrap();
        assert_eq!(budget.in_use(), messages.bytes() + gathered.bytes());
    }
}

#[test]
fn an_interrupt_cancels_the_gather() {
    struct Stop;
    impl Interrupts for Stop {
        fn check(&mut self) -> Result<(), Cancelled> {
            Err(Cancelled)
        }
    }
    let memory = memory();
    let messages = messages::<f64>(&memory, &SENT);
    let result = super::gather(
        workers(),
        &memory,
        &mut Stop,
        &example(false),
        &Gather::new(Combiner::Sum),
        &messages,
    );
    assert_eq!(result.unwrap_err(), ComputeError::Cancelled);
}

/// A random gather over a random graph.
#[derive(Debug, Clone)]
struct Scenario {
    id_space: u64,
    relation_count: usize,
    edges: Edges,
    reverse: bool,
    wide: bool,
    store_merged: bool,
    direction: Direction,
    selected: Option<Vec<usize>>,
    mask: Option<(Vec<f64>, bool)>,
    combiner: Combiner,
}

impl Scenario {
    /// The graph, with `u64` targets if `wide` (which the build otherwise
    /// chooses only for id spaces of 2³² or more).
    fn graph(&self) -> GraphCsr {
        let indexes = reference::relations(self.relation_count);
        let mut list = EdgeList::new(false);
        for &(relation, source, target, weight) in &self.edges {
            list.push(indexes[relation], source, target, weight).unwrap();
        }
        let (count, id_space) = (self.relation_count, self.id_space);
        let layout = CsrLayout { build_reverse: self.reverse, store_merged: self.store_merged };
        let graph = match self.wide {
            true => GraphCsr::build_with::<u64>(list, count, id_space, layout),
            false => GraphCsr::build_as(list, count, id_space, layout),
        };
        graph.unwrap()
    }

    fn naive<'a, T>(&'a self, sent: &'a [(DenseId, T)]) -> Naive<'a, T> {
        let selects = match &self.mask {
            None => vec![true; self.id_space as usize],
            Some((values, complement)) => {
                values.iter().map(|&value| naive::selects(value, *complement)).collect()
            }
        };
        Naive {
            id_space: self.id_space,
            edges: &self.edges,
            selected: self.selected.as_deref(),
            direction: self.direction,
            selects,
            sent,
        }
    }
}

fn scenarios() -> impl Strategy<Value = Scenario> {
    (1u64..14, 1usize..4).prop_flat_map(|(id_space, relation_count)| {
        let edge = (0..relation_count, 0..id_space, 0..id_space, Just(1.0));
        let mask_value = prop::sample::select(vec![0.0, -0.0, 1.0, f64::NAN]);
        (
            prop::collection::vec(edge, 0..50),
            any::<(bool, bool, bool)>(),
            prop_oneof![Just(Direction::In), Just(Direction::Out), Just(Direction::Both)],
            prop::option::of(prop::collection::vec(0..relation_count, 0..3)),
            prop::option::of((prop::collection::vec(mask_value, id_space as usize), any::<bool>())),
            prop::sample::select(Combiner::all().collect::<Vec<_>>()),
        )
            .prop_map(
                move |(
                    edges,
                    (reverse, wide, store_merged),
                    direction,
                    selected,
                    mask,
                    combiner,
                )| Scenario {
                    id_space,
                    relation_count,
                    edges,
                    reverse,
                    wide,
                    store_merged,
                    direction,
                    selected,
                    mask,
                    combiner,
                },
            )
    })
}

/// Senders mostly inside the id space (one in sixteen just outside it),
/// with messages drawn from `message`.
fn with_messages<T: Debug + Clone + 'static>(
    message: impl Strategy<Value = T> + Clone + 'static,
) -> impl Strategy<Value = (Scenario, Vec<(DenseId, T)>)> {
    scenarios().prop_flat_map(move |scenario| {
        let id_space = scenario.id_space;
        let sender = prop_oneof![15 => 0..id_space, 1 => Just(id_space)];
        (Just(scenario), prop::collection::vec((sender, message.clone()), 0..30))
    })
}

/// Runs the scenario through the kernel on `workers` and the reference.
fn check<M: Own>(
    workers: &Workers,
    s: &Scenario,
    sent: &[(DenseId, M::Owned)],
) -> Result<Rows<M::Owned>, TestCaseError> {
    let graph = s.graph();
    let indexes = reference::relations(s.relation_count);
    let selected: Option<Vec<RelationIndex>> =
        s.selected.as_ref().map(|r| r.iter().map(|&r| indexes[r]).collect());
    let mask = s.mask.as_ref().map(|(values, _)| vector(values.clone()));
    let gather = Gather {
        combiner: s.combiner,
        direction: s.direction,
        relations: selected.as_deref().map_or(RelationSelection::All, RelationSelection::Only),
        mask: mask
            .as_ref()
            .zip(s.mask.as_ref())
            .map(|(vector, &(_, complement))| Mask { vector, complement }),
    };
    let memory = memory();
    let mut messages = Messages::<M>::new(&memory).unwrap();
    for (sender, message) in sent {
        messages.push(*sender, M::view(message)).unwrap();
    }
    let found = super::gather(workers, &memory, &mut Uninterrupted, &graph, &gather, &messages)
        .map(|gathered| owned(&gathered));
    let expected = naive::gather::<M>(&s.naive(sent), s.combiner);
    match (&found, &expected) {
        (Ok(found), Ok(expected)) => prop_assert!(
            naive::same_rows::<M>(found, expected),
            "found {:?}, expected {:?}",
            found,
            expected
        ),
        _ => prop_assert_eq!(&found, &expected),
    }
    prop_assert_eq!(memory.in_use(), messages.bytes());
    Ok(found.unwrap_or_default())
}

fn f64s() -> impl Strategy<Value = f64> + Clone {
    prop_oneof![
        8 => (-4i8..5).prop_map(f64::from),
        1 => Just(f64::NAN),
        1 => Just(f64::INFINITY),
        1 => Just(-0.0),
        1 => Just(0.1),
    ]
}

proptest! {
    #[test]
    fn f64_gathers_match_the_naive_join((scenario, sent) in with_messages(f64s())) {
        check::<f64>(workers(), &scenario, &sent)?;
    }

    #[test]
    fn f32_gathers_match_the_naive_join(
        (scenario, sent) in with_messages(f64s().prop_map(|v| v as f32))
    ) {
        check::<f32>(workers(), &scenario, &sent)?;
    }

    #[test]
    fn i64_gathers_match_the_naive_join(
        (scenario, sent) in with_messages(prop_oneof![8 => -4i64..5, 1 => Just(i64::MAX), 1 => Just(i64::MIN)])
    ) {
        check::<i64>(workers(), &scenario, &sent)?;
    }

    #[test]
    fn bool_gathers_match_the_naive_join((scenario, sent) in with_messages(any::<bool>())) {
        check::<bool>(workers(), &scenario, &sent)?;
    }

    #[test]
    fn byte_gathers_match_the_naive_join(
        (scenario, sent) in with_messages(prop::collection::vec(any::<u8>(), 0..4))
    ) {
        check::<Bytes>(workers(), &scenario, &sent)?;
    }

    #[test]
    fn results_do_not_depend_on_the_thread_count((scenario, sent) in with_messages(f64s())) {
        static ONE: OnceLock<Workers> = OnceLock::new();
        let one = ONE.get_or_init(|| Workers::new(1).unwrap());
        let bits = |rows: Rows<f64>| -> Vec<(DenseId, Owned<u64>)> {
            rows.into_iter()
                .map(|(receiver, value)| (receiver, match value {
                    Owned::Count(count) => Owned::Count(count),
                    Owned::One(value) => Owned::One(value.to_bits()),
                    Owned::Many(values) => Owned::Many(values.iter().map(|v| v.to_bits()).collect()),
                }))
                .collect()
        };
        let on_one = bits(check::<f64>(one, &scenario, &sent)?);
        prop_assert_eq!(on_one, bits(check::<f64>(workers(), &scenario, &sent)?));
    }
}

#[test]
fn a_cursor_walks_the_rows_iter_yields() {
    let memory = memory();
    let sent = messages::<f64>(&memory, &SENT);
    let gathered = super::gather(
        workers(),
        &memory,
        &mut Uninterrupted,
        &example(true),
        &Gather::new(Combiner::Collect),
        &sent,
    )
    .unwrap();
    let mut at = RowAt::default();
    let mut walked = Vec::new();
    while let Some((receiver, received)) = gathered.next_row(&mut at) {
        let Received::Messages(messages) = received else { panic!("collect gives messages") };
        walked.push((receiver, messages.iter().collect::<Vec<_>>()));
    }
    assert_eq!(walked, [(0, vec![3.0, 4.0]), (1, vec![1.0]), (2, vec![1.0, 2.0])]);
    assert_eq!(gathered.next_row(&mut at).map(|(receiver, _)| receiver), None);
}
