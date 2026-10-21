//! The centralities against NetworkX's values on small graphs, and against
//! naive all-pairs references on random ones.

use std::sync::OnceLock;

use proptest::prelude::*;

use super::*;
use crate::compute::memory::{fully_held, Exact};
use crate::compute::reference::{self, Edges};
use crate::compute::{AnyVector, Mask};
use crate::graph::{CsrLayout, EdgeList, RelationIndex};
use crate::interrupts::Uninterrupted;

fn workers() -> &'static Workers {
    static WORKERS: OnceLock<Workers> = OnceLock::new();
    WORKERS.get_or_init(|| Workers::new(3).unwrap())
}

fn no_tombstones(id_space: u64) -> Tombstones {
    Tombstones::new(id_space).unwrap()
}

fn every_source() -> Sampling<'static> {
    Sampling { k: None, seed: 0, population: None }
}

fn mask(id_space: u64, selected: &[DenseId]) -> AnyVector {
    let values = (0..id_space).map(|node| selected.contains(&node)).collect();
    Vector::from_values(&ComputeMemory::new(1 << 20), id_space, 1, values).unwrap().into()
}

/// What a centrality gives over all relations of `graph`.
#[derive(Clone, Copy)]
enum Kind {
    Betweenness(BetweennessScale),
    Closeness(bool),
    Harmonic,
}

fn centrality(
    graph: &GraphCsr,
    tombstones: &Tombstones,
    directed: bool,
    sampling: &Sampling<'_>,
    kind: Kind,
) -> Result<Vec<f64>, ComputeError> {
    let memory = ComputeMemory::new(1 << 24);
    let paths = ShortestPaths {
        graph,
        relations: RelationSelection::All,
        tombstones,
        directed,
        weighted: None,
    };
    let found = match kind {
        Kind::Betweenness(scale) => {
            betweenness_centrality(workers(), &memory, &mut Uninterrupted, &paths, sampling, scale)
        }
        Kind::Closeness(wf) => {
            closeness_centrality(workers(), &memory, &mut Uninterrupted, &paths, sampling, wf)
        }
        Kind::Harmonic => {
            harmonic_centrality(workers(), &memory, &mut Uninterrupted, &paths, sampling)
        }
    }?;
    Ok(found.values.values().to_vec())
}

fn assert_close(found: &[f64], expected: &[f64]) {
    assert_eq!(found.len(), expected.len(), "{found:?} against {expected:?}");
    for (f, e) in found.iter().zip(expected) {
        assert!((f - e).abs() <= 1e-12 * e.abs().max(1.0), "{found:?} against {expected:?}");
    }
}

/// `0 → 1 → 2 → 3 → 0`, `0 → 2`, `1 → 3`, and node 4 alone.
fn small() -> GraphCsr {
    let edges: Edges = [(0, 1), (1, 2), (2, 3), (0, 2), (3, 0), (1, 3)]
        .iter()
        .map(|&(s, t)| (0, s, t, 1.0))
        .collect();
    reference::graph(5, 1, &edges, false, false)
}

#[test]
fn betweenness_is_networkx_s_exact_and_from_sampled_sources() {
    let graph = small();
    let none = no_tombstones(5);
    let selected = mask(5, &[0, 2]);
    let sampled = Sampling {
        k: None,
        seed: 0,
        population: Some(Mask { vector: &selected, complement: false }),
    };
    // nx.betweenness_centrality(G, normalized, endpoints), and with k=2
    // sampling [0, 2].
    for (normalized, endpoints, exact, from_two) in [
        (true, true, [0.45, 0.325, 0.325, 0.45, 0.0], [0.625, 0.3125, 0.5625, 0.5, 0.0]),
        (
            true,
            false,
            [0.25, 1.0 / 24.0, 1.0 / 24.0, 0.25, 0.0],
            [1.0 / 3.0, 1.0 / 12.0, 1.0 / 6.0, 1.0 / 3.0, 0.0],
        ),
        (false, true, [9.0, 6.5, 6.5, 9.0, 0.0], [12.5, 6.25, 11.25, 10.0, 0.0]),
        (false, false, [3.0, 0.5, 0.5, 3.0, 0.0], [4.0, 1.0, 2.0, 4.0, 0.0]),
    ] {
        let kind = Kind::Betweenness(BetweennessScale { normalized, endpoints });
        assert_close(&centrality(&graph, &none, true, &every_source(), kind).unwrap(), &exact);
        assert_close(&centrality(&graph, &none, true, &sampled, kind).unwrap(), &from_two);
    }
}

#[test]
fn closeness_and_harmonic_use_incoming_distances_as_networkx_does() {
    let mut edges: Edges = [(0, 1), (1, 2), (2, 3), (0, 2), (3, 0), (1, 3), (4, 1)]
        .iter()
        .map(|&(s, t)| (0, s, t, 1.0))
        .collect();
    edges.push((0, 2, 2, 1.0)); // A self-loop changes nothing.
    let graph = reference::graph(5, 1, &edges, false, false);
    let none = no_tombstones(5);
    let closeness = [0.5, 4.0 / 7.0, 2.0 / 3.0, 2.0 / 3.0, 0.0];
    for wf in [true, false] {
        let found = centrality(&graph, &none, true, &every_source(), Kind::Closeness(wf));
        assert_close(&found.unwrap(), &closeness);
    }
    let harmonic = [7.0 / 3.0, 17.0 / 6.0, 3.0, 3.0, 0.0];
    let found = centrality(&graph, &none, true, &every_source(), Kind::Harmonic);
    assert_close(&found.unwrap(), &harmonic);
    // nx.harmonic_centrality(G, sources=[0, 2]), scaled by (n - 1) / k_u.
    let selected = mask(5, &[0, 2]);
    let sampled = Sampling {
        k: None,
        seed: 0,
        population: Some(Mask { vector: &selected, complement: false }),
    };
    let found = centrality(&graph, &none, true, &sampled, Kind::Harmonic).unwrap();
    assert_close(&found, &[0.5 * 4.0, 4.0 / 3.0 * 2.0, 1.0 * 4.0, 1.5 * 2.0, 0.0]);
}

#[test]
fn weighted_paths_are_the_lightest() {
    let edges: Edges =
        vec![(0, 0, 1, 1.0), (0, 1, 2, 1.0), (0, 0, 2, 2.0), (0, 2, 3, 1.0), (0, 1, 3, 3.0)];
    let graph = reference::graph(4, 1, &edges, true, false);
    let none = no_tombstones(4);
    let scale = BetweennessScale { normalized: false, endpoints: false };
    let found = centrality(&graph, &none, true, &every_source(), Kind::Betweenness(scale));
    assert_close(&found.unwrap(), &[0.0, 1.0, 2.0, 0.0]);
    let found = centrality(&graph, &none, true, &every_source(), Kind::Closeness(true));
    assert_close(&found.unwrap(), &[0.0, 1.0 / 3.0, 4.0 / 9.0, 0.5]);
}

#[test]
fn a_weight_that_is_not_positive_is_refused() {
    for weight in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let graph = reference::graph(2, 1, &vec![(0, 0, 1, weight)], true, false);
        let found = centrality(&graph, &no_tombstones(2), true, &every_source(), Kind::Harmonic);
        assert_eq!(found, Err(ComputeError::NonPositiveWeight), "{weight}");
    }
}

#[test]
fn an_interrupt_cancels_the_searches() {
    struct Stop;
    impl Interrupts for Stop {
        fn check(&mut self) -> Result<(), Cancelled> {
            Err(Cancelled)
        }
    }
    let graph = small();
    let none = no_tombstones(5);
    let paths = ShortestPaths {
        graph: &graph,
        relations: RelationSelection::All,
        tombstones: &none,
        directed: true,
        weighted: None,
    };
    let memory = ComputeMemory::new(1 << 20);
    let found = harmonic_centrality(workers(), &memory, &mut Stop, &paths, &every_source());
    assert_eq!(found.map(|_| ()), Err(ComputeError::Cancelled));
    assert_eq!(memory.in_use(), 0);
}

#[test]
fn a_sample_gives_the_same_bits_on_any_pool() {
    let n = 300u64;
    let edges: Edges = (0..n)
        .flat_map(|i| [(0, i, (i * 7 + 1) % n, 1.0), (0, i, (i * 13 + 5) % n, 1.0)])
        .collect();
    let graph = reference::graph(n, 1, &edges, false, false);
    let none = no_tombstones(n);
    let sampling = Sampling { k: Some(20), seed: 9, population: None };
    let scale = BetweennessScale { normalized: true, endpoints: false };
    let on = |threads| {
        let workers = Workers::new(threads).unwrap();
        let memory = ComputeMemory::new(1 << 24);
        let paths = ShortestPaths {
            graph: &graph,
            relations: RelationSelection::All,
            tombstones: &none,
            directed: false,
            weighted: None,
        };
        let found =
            betweenness_centrality(&workers, &memory, &mut Uninterrupted, &paths, &sampling, scale)
                .unwrap();
        assert_eq!(found.sources, 20);
        found.values.values().iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    };
    assert_eq!(on(1), on(4));
}

#[test]
fn the_scratch_is_charged_and_held_memory_reclaimed_ahead() {
    let graph = small();
    let none = no_tombstones(5);
    let paths = ShortestPaths {
        graph: &graph,
        relations: RelationSelection::All,
        tombstones: &none,
        directed: false,
        weighted: None,
    };
    let scale = BetweennessScale { normalized: true, endpoints: false };
    let memory = ComputeMemory::new(200);
    let refused = betweenness_centrality(
        workers(),
        &memory,
        &mut Uninterrupted,
        &paths,
        &every_source(),
        scale,
    );
    assert!(matches!(refused, Err(ComputeError::MemoryLimit { .. })));
    assert_eq!(memory.in_use(), 0);
    let held = 1 << 20;
    let memory = ComputeMemory::reclaiming(held, held, held, Box::new(Exact));
    let found = betweenness_centrality(
        workers(),
        &memory,
        &mut Uninterrupted,
        &paths,
        &every_source(),
        scale,
    );
    assert!(found.is_ok());
}

/// Distances and shortest-path counts between every pair, the naive way,
/// over `a` (`None`: no edge), whose diagonal is ignored.
fn all_pairs(a: &[Vec<Option<f64>>]) -> (Vec<Vec<f64>>, Vec<Vec<f64>>) {
    let n = a.len();
    let mut d = vec![vec![f64::INFINITY; n]; n];
    for (i, row) in a.iter().enumerate() {
        d[i][i] = 0.0;
        for (j, &w) in row.iter().enumerate() {
            if let Some(w) = w.filter(|_| i != j) {
                d[i][j] = d[i][j].min(w);
            }
        }
    }
    for k in 0..n {
        for i in 0..n {
            for j in 0..n {
                d[i][j] = d[i][j].min(d[i][k] + d[k][j]);
            }
        }
    }
    let mut sigma = vec![vec![0.0; n]; n];
    for s in 0..n {
        let mut by_distance: Vec<usize> = (0..n).filter(|&t| d[s][t].is_finite()).collect();
        by_distance.sort_by(|&x, &y| d[s][x].total_cmp(&d[s][y]));
        sigma[s][s] = 1.0;
        for &t in by_distance.iter().skip(1) {
            sigma[s][t] = (0..n)
                .filter(|&v| v != t && a[v][t].is_some_and(|w| d[s][v] + w == d[s][t]))
                .map(|v| sigma[s][v])
                .sum();
        }
    }
    (d, sigma)
}

/// Unnormalised betweenness without endpoints from `sources`, summed over
/// ordered pairs.
fn naive_betweenness(d: &[Vec<f64>], sigma: &[Vec<f64>], sources: &[usize]) -> Vec<f64> {
    let n = d.len();
    let mut bc = vec![0.0; n];
    for &s in sources {
        for t in (0..n).filter(|&t| t != s && d[s][t].is_finite()) {
            for v in (0..n).filter(|&v| v != s && v != t) {
                if d[s][v] + d[v][t] == d[s][t] {
                    bc[v] += sigma[s][v] * sigma[v][t] / sigma[s][t];
                }
            }
        }
    }
    bc
}

type Scenario = (u64, usize, Edges, Vec<DenseId>, bool, bool, Vec<DenseId>);

fn scenarios() -> impl Strategy<Value = Scenario> {
    (1u64..9, 1usize..3).prop_flat_map(|(id_space, relations)| {
        let edge = (0..relations, 0..id_space, 0..id_space, (1u8..4).prop_map(f64::from));
        (
            Just(id_space),
            Just(relations),
            prop::collection::vec(edge, 0..24),
            prop::collection::vec(0..id_space, 0..2),
            any::<bool>(),
            any::<bool>(),
            prop::collection::vec(0..id_space, 0..4),
        )
    })
}

/// The scenario's graph, without the edges of its tombstoned nodes, and
/// the dense matrix the centralities read.
fn build(
    (id_space, relations, edges, tombstoned, weighted, directed, _): &Scenario,
) -> (GraphCsr, Tombstones, Vec<Vec<Option<f64>>>) {
    let edges: Edges = edges
        .iter()
        .copied()
        .filter(|&(_, s, t, _)| !tombstoned.contains(&s) && !tombstoned.contains(&t))
        .collect();
    let indexes: Vec<RelationIndex> = reference::relations(*relations);
    let mut list = EdgeList::new(*weighted);
    for &(r, s, t, w) in &edges {
        list.push(indexes[r], s, t, w).unwrap();
    }
    let layout = CsrLayout { build_reverse: false, store_merged: true };
    let graph = GraphCsr::build_as(list, *relations, *id_space, layout).unwrap();
    let mut tombstones = Tombstones::new(*id_space).unwrap();
    tombstoned.iter().for_each(|&t| tombstones.insert(t));
    let mut a = reference::adjacency(*id_space, &edges, None, *weighted);
    if !directed {
        let pairs: Vec<(usize, usize)> =
            (0..a.len()).flat_map(|i| (0..i).map(move |j| (i, j))).collect();
        for (i, j) in pairs {
            let lighter = match (a[i][j], a[j][i]) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (x, y) => x.or(y),
            };
            (a[i][j], a[j][i]) = (lighter, lighter);
        }
    }
    (graph, tombstones, a)
}

proptest! {
    #[test]
    fn betweenness_matches_the_naive_pair_sums(scenario in scenarios()) {
        let (graph, tombstones, a) = build(&scenario);
        let (d, sigma) = all_pairs(&a);
        let live: Vec<usize> = (0..scenario.0).filter(|&n| !tombstones.contains(n)).map(|n| n as usize).collect();
        let directed = scenario.5;
        let scale = BetweennessScale { normalized: false, endpoints: false };
        let found = centrality(&graph, &tombstones, directed, &every_source(), Kind::Betweenness(scale)).unwrap();
        let halve = if directed { 1.0 } else { 0.5 };
        let expected: Vec<f64> = naive_betweenness(&d, &sigma, &live).iter().map(|b| b * halve).collect();
        assert_close(&found, &expected);
    }

    #[test]
    fn sampled_closeness_and_harmonic_match_the_naive_estimates(scenario in scenarios()) {
        let (graph, tombstones, a) = build(&scenario);
        let (d, _) = all_pairs(&a);
        let n = scenario.0;
        let live: Vec<u64> = (0..n).filter(|&v| !tombstones.contains(v)).collect();
        let mut sources: Vec<u64> = scenario.6.iter().copied().filter(|s| live.contains(s)).collect();
        sources.sort_unstable();
        sources.dedup();
        let selected = mask(n, &sources);
        let sampling = Sampling { k: None, seed: 0, population: Some(Mask { vector: &selected, complement: false }) };
        let directed = scenario.5;
        let (mut closeness, mut harmonic) = (vec![0.0; n as usize], vec![0.0; n as usize]);
        for &u in &live {
            let others: Vec<u64> = sources.iter().copied().filter(|&s| s != u).collect();
            let reaching: Vec<f64> = others.iter().map(|&s| d[s as usize][u as usize]).filter(|d| d.is_finite()).collect();
            let (m, total) = (reaching.len() as f64, reaching.iter().sum::<f64>());
            if total > 0.0 && live.len() > 1 {
                closeness[u as usize] = m / total * (m / others.len() as f64);
            }
            if !others.is_empty() {
                let sum: f64 = reaching.iter().map(|d| 1.0 / d).sum();
                harmonic[u as usize] = sum * ((live.len() - 1) as f64 / others.len() as f64);
            }
        }
        let every = sources.len() == live.len();
        let found = centrality(&graph, &tombstones, directed, &sampling, Kind::Closeness(true)).unwrap();
        assert_close(&found, &closeness);
        let found = centrality(&graph, &tombstones, directed, &sampling, Kind::Harmonic).unwrap();
        if every {
            // Exact: not rescaled at all.
            let exact: Vec<f64> = (0..n as usize).map(|u| (0..n as usize).filter(|&v| v != u && d[v][u].is_finite() && live.contains(&(v as u64))).map(|v| 1.0 / d[v][u]).sum()).collect();
            assert_close(&found, &exact);
        } else {
            assert_close(&found, &harmonic);
        }
    }
}

#[test]
fn every_search_fits_a_budget_that_is_all_held_memory() {
    // Weighted and not, directed and not, one relation and two (a row
    // scratch), from every source and a sample.
    let edges: Edges = (0..40u64)
        .flat_map(|i| [(0, i, (i * 7 + 1) % 40, 1.0 + (i % 3) as f64), (1, i, (i + 3) % 40, 2.0)])
        .collect();
    let none = no_tombstones(40);
    for (weighted, relations) in [(false, 1), (true, 1), (true, 2)] {
        let kept: Edges = edges.iter().copied().filter(|&(r, ..)| r < relations).collect();
        let graph = reference::graph(40, relations, &kept, weighted, false);
        for directed in [true, false] {
            let paths = ShortestPaths {
                graph: &graph,
                relations: RelationSelection::All,
                tombstones: &none,
                directed,
                weighted: None,
            };
            for sampling in [every_source(), Sampling { k: Some(5), seed: 1, population: None }] {
                let scale = BetweennessScale { normalized: true, endpoints: true };
                let memory = fully_held();
                betweenness_centrality(
                    workers(),
                    &memory,
                    &mut Uninterrupted,
                    &paths,
                    &sampling,
                    scale,
                )
                .unwrap();
                closeness_centrality(
                    workers(),
                    &fully_held(),
                    &mut Uninterrupted,
                    &paths,
                    &sampling,
                    true,
                )
                .unwrap();
                harmonic_centrality(
                    workers(),
                    &fully_held(),
                    &mut Uninterrupted,
                    &paths,
                    &sampling,
                )
                .unwrap();
            }
        }
    }
}

#[test]
fn a_distance_that_overflows_is_refused() {
    let edges: Edges = vec![(0, 0, 1, 1e308), (0, 1, 2, 1e308), (0, 0, 2, 1e308)];
    let graph = reference::graph(3, 1, &edges, true, false);
    let found = centrality(&graph, &no_tombstones(3), true, &every_source(), Kind::Harmonic);
    assert_eq!(found, Err(ComputeError::NonPositiveWeight));
}

#[test]
fn a_self_loop_s_weight_is_never_read() {
    let edges: Edges = vec![(0, 0, 0, 0.0), (0, 0, 1, 2.0), (0, 1, 1, -1.0)];
    let graph = reference::graph(2, 1, &edges, true, false);
    let scale = BetweennessScale { normalized: false, endpoints: false };
    let found =
        centrality(&graph, &no_tombstones(2), true, &every_source(), Kind::Betweenness(scale));
    assert_close(&found.unwrap(), &[0.0, 0.0]);
    let found = centrality(&graph, &no_tombstones(2), true, &every_source(), Kind::Harmonic);
    assert_close(&found.unwrap(), &[0.0, 0.5]);
}
