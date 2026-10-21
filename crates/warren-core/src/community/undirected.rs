//! The undirected weighted graph that modularity is defined on (DESIGN.md
//! §6.7): the symmetric matrix `B` whose entry `B[u][v]` is the weight
//! joining `u` and `v` whatever the direction. For a directed graph
//! `B = A + Aᵀ` (an edge and its reverse add up, so a self-loop counts
//! twice); an undirected graph is stored both ways with a self-loop once,
//! and `B = A`: a self-loop counts once, as in pg_warren's degrees and
//! `contract` (§6.6), where NetworkX counts it twice. Every relation's
//! edges add up, as in the merged CSR. A node's degree `k` is its row sum
//! and `2m` the sum of all of `B`.
//!
//! Both forms are closed under contraction (§6.6): the `B` of a quotient
//! is `PᵀBP`, so a partition's modularity on a graph is that of the
//! singletons on its quotient.

use std::mem::size_of;

use rayon::prelude::*;

use super::CommunityError;
use crate::compute::{ComputeError, ComputeMemory, MemoryCharge};
use crate::graph::{DenseId, GraphCsr};
use crate::interrupts::{Cancellation, Cancelled};

/// The rows one task folds; small under test, so tests cross tasks.
const NODES_PER_TASK: usize = if cfg!(test) { 3 } else { 4096 };

/// How often the sequential passes check the cancellation flag, in nodes.
const NODES_PER_CHECK: u64 = 1 << 16;

/// `B` as rows of `(neighbour, weight)`, each row ascending and distinct,
/// with each node's degree and their total.
pub struct Undirected {
    offsets: Vec<u64>,
    entries: Vec<(DenseId, f64)>,
    degrees: Vec<f64>,
    total: f64,
    _charges: [MemoryCharge; 3],
}

impl Undirected {
    /// At most the bytes [`Self::new`] charges for `graph`.
    pub fn bytes(graph: &GraphCsr, directed: bool) -> u64 {
        let stored: u64 = graph.relations().iter().map(|r| r.forward().edge_count()).sum();
        let entries = if directed { stored.saturating_mul(2) } else { stored };
        let nodes = graph.all_relations().forward().id_space();
        let per_node = (2 * size_of::<u64>()) as u64;
        entries
            .saturating_mul(size_of::<(DenseId, f64)>() as u64)
            .saturating_add(nodes.saturating_add(1).saturating_mul(per_node))
    }

    /// `B` of `graph`'s relations, each edge weighing its weight if
    /// `weighted` (else 1), charged to `memory`. Refuses a negative or NaN
    /// weight: modularity needs weights of at least 0.
    pub fn new(
        memory: &ComputeMemory,
        cancellation: &Cancellation,
        graph: &GraphCsr,
        weighted: bool,
        directed: bool,
    ) -> Result<Self, CommunityError> {
        let nodes = graph.all_relations().forward().id_space();
        let len =
            usize::try_from(nodes).map_err(|_| ComputeError::OutOfMemory { bytes: u64::MAX })?;
        let (mut offsets, offsets_charge) = memory.filled(len + 1, 0u64)?;
        each_entry(graph, weighted, directed, cancellation, &mut |row, _, _| {
            offsets[row as usize] += 1;
        })?;
        let mut end = 0;
        for offset in &mut offsets[..len] {
            end += *offset;
            *offset = end;
        }
        offsets[len] = end;
        let (mut entries, entries_charge) = memory.filled(end as usize, (0, 0.0))?;
        let mut negative = None;
        each_entry(graph, weighted, directed, cancellation, &mut |row, target, weight| {
            if (weight.is_nan() || weight < 0.0) && negative.is_none() {
                negative = Some((row, target, weight));
            }
            let offset = &mut offsets[row as usize];
            *offset -= 1;
            entries[*offset as usize] = (target, weight);
        })?;
        if let Some((source, target, weight)) = negative {
            return Err(CommunityError::NegativeWeight { from: source, to: target, weight });
        }
        fold_rows(&mut offsets, &mut entries, cancellation)?;
        let (mut degrees, degrees_charge) = memory.filled(len, 0.0)?;
        for (node, degree) in degrees.iter_mut().enumerate() {
            let row = &entries[offsets[node] as usize..offsets[node + 1] as usize];
            *degree = row.iter().map(|&(_, weight)| weight).sum();
        }
        let total = degrees.iter().sum();
        Ok(Self {
            offsets,
            entries,
            degrees,
            total,
            _charges: [offsets_charge, entries_charge, degrees_charge],
        })
    }

    /// The number of rows: the version's id space.
    pub fn id_space(&self) -> u64 {
        self.degrees.len() as u64
    }

    /// The row of `node`: its neighbours, ascending, with `B`'s weights.
    pub fn row(&self, node: DenseId) -> &[(DenseId, f64)] {
        let node = node as usize;
        &self.entries[self.offsets[node] as usize..self.offsets[node + 1] as usize]
    }

    /// The degree `k` of `node`: its row's sum.
    pub fn degree(&self, node: DenseId) -> f64 {
        self.degrees[node as usize]
    }

    /// Every node's degree.
    pub fn degrees(&self) -> &[f64] {
        &self.degrees
    }

    /// `2m`: the sum of every degree.
    pub fn total(&self) -> f64 {
        self.total
    }
}

/// Passes each entry of `B` to `entry` as (row, target, weight), in a
/// fixed order: relation by relation, row by row; a directed edge `u → v`
/// to rows `u` and `v` (a self-loop once, with twice its weight), an
/// undirected graph's stored entries as they are.
fn each_entry(
    graph: &GraphCsr,
    weighted: bool,
    directed: bool,
    cancellation: &Cancellation,
    entry: &mut dyn FnMut(DenseId, DenseId, f64),
) -> Result<(), Cancelled> {
    for relation in graph.relations() {
        let csr = relation.forward();
        for source in 0..csr.id_space() {
            if source % NODES_PER_CHECK == 0 && cancellation.is_cancelled() {
                return Err(Cancelled);
            }
            let weights = csr.neighbour_weights(source).filter(|_| weighted);
            for (at, target) in csr.neighbours(source).enumerate() {
                let weight = weights.map_or(1.0, |weights| weights[at]);
                match (source == target, directed) {
                    (true, true) => entry(source, target, 2.0 * weight),
                    (false, true) => {
                        entry(source, target, weight);
                        entry(target, source, weight);
                    }
                    (_, false) => entry(source, target, weight),
                }
            }
        }
    }
    Ok(())
}

/// Sorts each row by target (stably, so equal targets keep their order)
/// and folds equal targets into one entry, summing their weights in that
/// order; then closes the gaps the folding left and fixes the offsets.
fn fold_rows(
    offsets: &mut [u64],
    entries: &mut [(DenseId, f64)],
    cancellation: &Cancellation,
) -> Result<(), CommunityError> {
    let nodes = offsets.len() - 1;
    let mut folded_lengths = vec![0u64; 0];
    let mut tasks = Vec::new();
    let mut rest = &mut *entries;
    for first in (0..nodes).step_by(NODES_PER_TASK) {
        let last = nodes.min(first + NODES_PER_TASK);
        let len = (offsets[last] - offsets[first]) as usize;
        let (chunk, tail) = std::mem::take(&mut rest).split_at_mut(len);
        tasks.push((first..last, chunk));
        rest = tail;
    }
    let offsets_read = &*offsets;
    let lengths: Vec<Vec<u64>> = tasks
        .into_par_iter()
        .map(|(rows, chunk)| {
            if cancellation.is_cancelled() {
                return Err(Cancelled);
            }
            let base = offsets_read[rows.start];
            Ok(rows
                .map(|row| {
                    let range = (offsets_read[row] - base) as usize
                        ..(offsets_read[row + 1] - base) as usize;
                    fold_row(&mut chunk[range])
                })
                .collect())
        })
        .collect::<Result<_, Cancelled>>()?;
    folded_lengths.extend(lengths.into_iter().flatten());
    let mut write = 0usize;
    for (row, &len) in folded_lengths.iter().enumerate() {
        let start = offsets[row] as usize;
        entries.copy_within(start..start + len as usize, write);
        offsets[row] = write as u64;
        write += len as usize;
    }
    offsets[nodes] = write as u64;
    Ok(())
}

/// Sorts `row` stably by target, folds equal targets in place, and
/// returns the folded length.
fn fold_row(row: &mut [(DenseId, f64)]) -> u64 {
    row.sort_by_key(|&(target, _)| target);
    let mut kept = 0;
    for at in 0..row.len() {
        if kept > 0 && row[kept - 1].0 == row[at].0 {
            row[kept - 1].1 += row[at].1;
        } else {
            row[kept] = row[at];
            kept += 1;
        }
    }
    kept as u64
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::compute::reference::{self, Edges};

    fn undirected(graph: &GraphCsr, weighted: bool, directed: bool) -> Undirected {
        let memory = ComputeMemory::new(u64::MAX);
        Undirected::new(&memory, &Cancellation::new(), graph, weighted, directed).unwrap()
    }

    /// `B` the dense way, from the stored entries.
    fn naive(graph: &GraphCsr, weighted: bool, directed: bool) -> Vec<Vec<f64>> {
        let n = graph.all_relations().forward().id_space() as usize;
        let mut b = vec![vec![0.0; n]; n];
        for relation in graph.relations() {
            let csr = relation.forward();
            for u in 0..n as u64 {
                let weights = csr.neighbour_weights(u);
                for (at, v) in csr.neighbours(u).enumerate() {
                    let w = match (weighted, weights) {
                        (true, Some(weights)) => weights[at],
                        _ => 1.0,
                    };
                    b[u as usize][v as usize] += w;
                    if directed {
                        b[v as usize][u as usize] += w;
                    }
                }
            }
        }
        b
    }

    #[test]
    fn a_directed_edge_and_its_reverse_add_up_and_a_self_loop_counts_twice() {
        let edges: Edges = vec![(0, 0, 1, 2.0), (0, 1, 0, 3.0), (0, 1, 2, 1.0), (0, 2, 2, 4.0)];
        let graph = reference::graph(3, 1, &edges, true, false);
        let b = undirected(&graph, true, true);
        assert_eq!(b.row(0), [(1, 5.0)]);
        assert_eq!(b.row(1), [(0, 5.0), (2, 1.0)]);
        assert_eq!(b.row(2), [(1, 1.0), (2, 8.0)]);
        assert_eq!((b.degree(0), b.degree(1), b.degree(2)), (5.0, 6.0, 9.0));
        assert_eq!(b.total(), 20.0);
        let unweighted = undirected(&graph, false, true);
        assert_eq!(unweighted.row(2), [(1, 1.0), (2, 2.0)]);
    }

    #[test]
    fn an_undirected_graph_is_already_symmetric_and_a_self_loop_counts_once() {
        let edges: Edges = vec![(0, 0, 1, 2.0), (0, 1, 0, 2.0), (0, 1, 1, 3.0)];
        let graph = reference::graph(2, 1, &edges, true, false);
        let b = undirected(&graph, true, false);
        assert_eq!(b.row(0), [(1, 2.0)]);
        assert_eq!(b.row(1), [(0, 2.0), (1, 3.0)]);
        assert_eq!(b.total(), 7.0);
    }

    #[test]
    fn a_negative_or_nan_weight_is_refused() {
        for bad in [-1.0, f64::NAN] {
            let graph = reference::graph(2, 1, &vec![(0, 0, 1, bad)], true, false);
            let memory = ComputeMemory::new(u64::MAX);
            let refused = Undirected::new(&memory, &Cancellation::new(), &graph, true, true);
            assert!(matches!(refused, Err(CommunityError::NegativeWeight { from: 0, to: 1, .. })));
        }
    }

    #[test]
    fn it_is_charged_to_the_budget_within_its_bound() {
        let edges: Edges = (0..40u64).map(|i| (0, i % 7, (i * 3) % 7, 1.0)).collect();
        let graph = reference::graph(7, 1, &edges, true, false);
        let memory = ComputeMemory::new(u64::MAX);
        let b = Undirected::new(&memory, &Cancellation::new(), &graph, true, true).unwrap();
        assert!(memory.in_use() <= Undirected::bytes(&graph, true));
        drop(b);
        let small = ComputeMemory::new(64);
        assert!(matches!(
            Undirected::new(&small, &Cancellation::new(), &graph, true, true),
            Err(CommunityError::Compute(ComputeError::MemoryLimit { .. }))
        ));
    }

    proptest! {
        #[test]
        fn rows_hold_b(
            (n, edges) in (1u64..12).prop_flat_map(|n| (
                Just(n),
                prop::collection::vec((0usize..2, 0..n, 0..n, 1..5u32), 0..40),
            )),
            weighted in any::<bool>(),
            directed in any::<bool>(),
        ) {
            let edges: Edges =
                edges.into_iter().map(|(r, s, t, w)| (r, s, t, f64::from(w))).collect();
            let graph = reference::graph(n, 2, &edges, true, false);
            let b = undirected(&graph, weighted, directed);
            let expected = naive(&graph, weighted, directed);
            for u in 0..n {
                let row: Vec<(u64, f64)> = expected[u as usize]
                    .iter()
                    .enumerate()
                    .filter(|&(_, &w)| w != 0.0)
                    .map(|(v, &w)| (v as u64, w))
                    .collect();
                prop_assert_eq!(b.row(u), &row[..]);
                prop_assert_eq!(b.degree(u), row.iter().map(|&(_, w)| w).sum::<f64>());
            }
        }
    }
}
