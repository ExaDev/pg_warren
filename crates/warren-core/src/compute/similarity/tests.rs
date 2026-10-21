//! The similarities against NetworkX's values on a small graph, and
//! against naive set arithmetic on random ones, read both ways (node by
//! node through reverse CSRs, and from the built symmetric adjacency).

use std::collections::BTreeSet;
use std::sync::OnceLock;

use proptest::prelude::*;

use super::*;
use crate::compute::reference::{self, Edges};
use crate::compute::{AnyVector, ComputeMemory, Mask, Vector, Workers};
use crate::graph::{CsrLayout, DenseId, EdgeList};
use crate::interrupts::{Cancelled, Interrupts, Uninterrupted};

const MEASURES: [Similarity; 3] =
    [Similarity::Jaccard, Similarity::AdamicAdar, Similarity::CommonNeighbours];

fn workers() -> &'static Workers {
    static WORKERS: OnceLock<Workers> = OnceLock::new();
    WORKERS.get_or_init(|| Workers::new(3).unwrap())
}

fn all(graph: &GraphCsr) -> Neighbourhood<'_, '_> {
    Neighbourhood { graph, relations: RelationSelection::All }
}

fn pairs(
    graph: &Neighbourhood<'_, '_>,
    measure: Similarity,
    pairs: &[(DenseId, DenseId)],
) -> Vec<f64> {
    let memory = ComputeMemory::new(1 << 24);
    similarity_of_pairs(workers(), &memory, &mut Uninterrupted, graph, measure, pairs)
        .unwrap()
        .values
}

fn top(
    graph: &Neighbourhood<'_, '_>,
    measure: Similarity,
    node: DenseId,
    k: u64,
) -> Vec<(DenseId, f64)> {
    let memory = ComputeMemory::new(1 << 24);
    let found =
        most_similar(workers(), &memory, &mut Uninterrupted, graph, measure, node, k).unwrap();
    found.into_iter().map(|similar| (similar.node, similar.score)).collect()
}

fn each(
    graph: &Neighbourhood<'_, '_>,
    measure: Similarity,
    k: u64,
    nodes: Option<Mask<'_>>,
) -> Vec<(DenseId, DenseId, f64)> {
    let memory = ComputeMemory::new(1 << 24);
    let found =
        similar_pairs(workers(), &memory, &mut Uninterrupted, graph, measure, k, nodes).unwrap();
    found.tasks.into_iter().flatten().map(|(u, similar)| (u, similar.node, similar.score)).collect()
}

fn assert_close(found: &[f64], expected: &[f64]) {
    assert_eq!(found.len(), expected.len(), "{found:?} against {expected:?}");
    for (f, e) in found.iter().zip(expected) {
        let close = f == e || (f - e).abs() <= 1e-12 * e.abs().max(1.0);
        assert!(close, "{found:?} against {expected:?}");
    }
}

/// NetworkX's `Graph([(0,1),(0,2),(1,2),(2,3),(3,4),(1,3),(4,5)])`, stored
/// one way and the other, with a self-loop, in two relations.
fn small(reverse: bool) -> GraphCsr {
    let edges: Edges = vec![
        (0, 0, 1, 1.0),
        (1, 2, 0, 1.0),
        (0, 1, 2, 1.0),
        (1, 2, 3, 1.0),
        (0, 4, 3, 1.0),
        (1, 1, 3, 1.0),
        (0, 3, 1, 1.0),
        (0, 4, 5, 1.0),
        (1, 2, 2, 1.0),
    ];
    reference::graph(6, 2, &edges, false, reverse)
}

#[test]
fn pairs_score_as_networkx_does() {
    for reverse in [true, false] {
        let graph = small(reverse);
        let graph = all(&graph);
        let asked = [(0, 3), (0, 4), (2, 4), (5, 0), (1, 1)];
        let jaccard = pairs(&graph, Similarity::Jaccard, &asked);
        assert_close(&jaccard, &[2.0 / 3.0, 0.0, 0.25, 0.0, 1.0]);
        let adamic_adar = pairs(&graph, Similarity::AdamicAdar, &asked[..3]);
        assert_close(&adamic_adar, &[1.8204784532536746, 0.0, 0.9102392266268373]);
        let common = pairs(&graph, Similarity::CommonNeighbours, &asked);
        assert_close(&common, &[2.0, 0.0, 1.0, 0.0, 3.0]);
    }
}

#[test]
fn a_node_paired_with_itself_shares_a_leaf_s_infinite_term() {
    // Node 5's only neighbour is 4, whose degree is 2; node 4's
    // neighbour 5 has degree 1: ln 1 = 0.
    let graph = small(true);
    let scores = pairs(&all(&graph), Similarity::AdamicAdar, &[(5, 5), (4, 4)]);
    assert_eq!(scores[0], 1.0 / 2f64.ln());
    assert_eq!(scores[1], f64::INFINITY);
}

#[test]
fn the_most_similar_exclude_the_node_and_its_neighbours() {
    for reverse in [true, false] {
        let graph = small(reverse);
        let graph = all(&graph);
        // Node 0's non-neighbours sharing a neighbour: 3 (via 1, 2).
        assert_eq!(top(&graph, Similarity::CommonNeighbours, 0, 5), vec![(3, 2.0)]);
        // Node 4: 1 and 2 (via 3), tied, by dense id.
        assert_eq!(top(&graph, Similarity::CommonNeighbours, 4, 1), vec![(1, 1.0)]);
        assert_eq!(top(&graph, Similarity::Jaccard, 4, 5), vec![(1, 0.25), (2, 0.25)]);
    }
}

#[test]
fn an_unknown_measure_is_refused() {
    let refused = "jacard".parse::<Similarity>();
    assert_eq!(refused, Err(ComputeError::UnknownSimilarity("jacard".into())));
    assert_eq!("adamic_adar".parse(), Ok(Similarity::AdamicAdar));
}

#[test]
fn an_interrupt_cancels_a_ranking() {
    struct Stop;
    impl Interrupts for Stop {
        fn check(&mut self) -> Result<(), Cancelled> {
            Err(Cancelled)
        }
    }
    let graph = small(false);
    let memory = ComputeMemory::new(1 << 24);
    let graph = all(&graph);
    let found = similar_pairs(workers(), &memory, &mut Stop, &graph, Similarity::Jaccard, 3, None);
    assert_eq!(found.map(|_| ()), Err(ComputeError::Cancelled));
    assert_eq!(memory.in_use(), 0);
}

#[test]
fn the_tallies_are_charged() {
    let graph = small(false);
    let memory = ComputeMemory::new(64);
    let graph = all(&graph);
    let found =
        similar_pairs(workers(), &memory, &mut Uninterrupted, &graph, Similarity::Jaccard, 3, None);
    assert!(matches!(found, Err(ComputeError::MemoryLimit { .. })));
    assert_eq!(memory.in_use(), 0);
}

#[test]
fn a_hub_shared_by_many_pairs_is_searched_not_merged() {
    // A star of 100,000 leaves: every pair of leaves shares the hub.
    let n = 100_000;
    let edges: Edges = (1..=n).map(|leaf| (0, 0, leaf, 1.0)).collect();
    for reverse in [true, false] {
        let graph = reference::graph(n + 1, 1, &edges, false, reverse);
        let asked: Vec<_> = (1..n).map(|leaf| (leaf, leaf + 1)).collect();
        let scores = pairs(&all(&graph), Similarity::Jaccard, &asked);
        assert!(scores.iter().all(|&s| s == 1.0));
    }
}

/// The undirected simple graph of `edges` in `selected` (all if `None`).
fn neighbours(id_space: u64, edges: &Edges, selected: Option<&[usize]>) -> Vec<BTreeSet<DenseId>> {
    let mut sets = vec![BTreeSet::new(); id_space as usize];
    for &(relation, s, t, _) in edges {
        if s != t && selected.is_none_or(|selected| selected.contains(&relation)) {
            sets[s as usize].insert(t);
            sets[t as usize].insert(s);
        }
    }
    sets
}

fn naive(sets: &[BTreeSet<DenseId>], measure: Similarity, u: DenseId, v: DenseId) -> f64 {
    let (a, b) = (&sets[u as usize], &sets[v as usize]);
    let common: Vec<_> = a.intersection(b).collect();
    match measure {
        Similarity::CommonNeighbours => common.len() as f64,
        Similarity::Jaccard => match a.union(b).count() {
            0 => 0.0,
            union => common.len() as f64 / union as f64,
        },
        Similarity::AdamicAdar => {
            common.iter().map(|&&w| 1.0 / (sets[w as usize].len() as f64).ln()).sum()
        }
    }
}

/// Node `u`'s ranking, naively: every non-neighbour other than `u` that
/// shares a neighbour, best first, the first `k`.
fn naive_top(
    sets: &[BTreeSet<DenseId>],
    measure: Similarity,
    u: DenseId,
    k: u64,
) -> Vec<(DenseId, f64)> {
    let mut ranked: Vec<Similar> = (0..sets.len() as u64)
        .filter(|&v| v != u && !sets[u as usize].contains(&v))
        .filter(|&v| sets[u as usize].intersection(&sets[v as usize]).next().is_some())
        .map(|v| Similar { node: v, score: naive(sets, measure, u, v) })
        .collect();
    ranked.sort();
    ranked.into_iter().take(k as usize).map(|s| (s.node, s.score)).collect()
}

type Scenario = (u64, usize, Edges, Option<Vec<usize>>, (bool, bool), u64, Vec<bool>);

fn scenarios() -> impl Strategy<Value = Scenario> {
    (1u64..14, 1usize..4).prop_flat_map(|(id_space, relations)| {
        let edge = (0..relations, 0..id_space, 0..id_space, Just(1.0));
        (
            Just(id_space),
            Just(relations),
            prop::collection::vec(edge, 0..50),
            prop::option::of(prop::collection::vec(0..relations, 0..3)),
            any::<(bool, bool)>(),
            1u64..5,
            prop::collection::vec(any::<bool>(), id_space as usize),
        )
    })
}

proptest! {
    #[test]
    fn every_form_matches_the_naive_sets(
        (id_space, relations, edges, selected, (reverse, store_merged), k, chosen) in scenarios()
    ) {
        let indexes = reference::relations(relations);
        let mut list = EdgeList::new(false);
        for &(r, s, t, w) in &edges {
            list.push(indexes[r], s, t, w).unwrap();
        }
        let layout = CsrLayout { build_reverse: reverse, store_merged };
        let graph = GraphCsr::build_as(list, relations, id_space, layout).unwrap();
        let named: Option<Vec<_>> = selected.as_ref().map(|s| s.iter().map(|&r| indexes[r]).collect());
        let relations = named.as_deref().map_or(RelationSelection::All, RelationSelection::Only);
        let graph = Neighbourhood { graph: &graph, relations };
        let sets = neighbours(id_space, &edges, selected.as_deref());
        let every: Vec<(DenseId, DenseId)> =
            (0..id_space).flat_map(|u| (0..id_space).map(move |v| (u, v))).collect();
        let memory = ComputeMemory::new(1 << 20);
        let chosen_mask: AnyVector =
            Vector::from_values(&memory, id_space, 1, chosen.clone()).unwrap().into();
        for measure in MEASURES {
            let expected: Vec<f64> = every.iter().map(|&(u, v)| naive(&sets, measure, u, v)).collect();
            assert_close(&pairs(&graph, measure, &every), &expected);
            let mut expected_each = Vec::new();
            for u in 0..id_space {
                let ranked = naive_top(&sets, measure, u, k);
                prop_assert_eq!(top(&graph, measure, u, k), ranked.clone());
                if chosen[u as usize] {
                    expected_each.extend(ranked.into_iter().map(|(v, s)| (u, v, s)));
                }
            }
            let mask = Mask { vector: &chosen_mask, complement: false };
            prop_assert_eq!(each(&graph, measure, k, Some(mask)), expected_each);
        }
    }
}

#[test]
fn k_may_exceed_the_id_space() {
    let graph = small(true);
    let graph = all(&graph);
    assert_eq!(top(&graph, Similarity::CommonNeighbours, 0, u64::MAX), vec![(3, 2.0)]);
    let found = each(&graph, Similarity::CommonNeighbours, u64::MAX, None);
    assert_eq!(found.len(), 8);
}
