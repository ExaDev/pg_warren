//! Modularity (DESIGN.md §6.7): how much more weight a partition keeps
//! inside its communities than a random graph with the same degrees would,
//! `Q = Σ_c [ in_c / 2m − γ (K_c / 2m)² ]`, with `in_c` the sum of `B`
//! over pairs of members of `c` (each undirected edge twice, a self-loop
//! as [`Undirected`] counts it), `K_c` the sum of its members' degrees and
//! `γ` the resolution. It is 0 for a graph without edges. On an undirected
//! graph it is NetworkX's `community.modularity` of the same graph with
//! each self-loop's weight halved (NetworkX counts a self-loop twice in its
//! node's degree, pg_warren once), so the two agree without self-loops. On
//! a directed graph it is NetworkX's modularity of the undirected
//! multigraph of its edges (`nx.MultiGraph(G)`), not of `G`: NetworkX
//! scores a directed graph with Leicht and Newman's directed formula.

use super::{CommunityError, Undirected};
use crate::compute::{ComputeError, ComputeMemory};
use crate::graph::DenseId;

/// The bytes per node [`modularity`] charges: the grouping's order and
/// index, and each community's two sums.
pub(super) const GROUPING_BYTES_PER_NODE: u64 = 32;

/// The modularity of the partition that gives node `n` the label
/// `labels[n]`, at resolution `resolution`. Nodes without edges (tombstoned
/// ones among them) add nothing, whatever their label.
pub fn modularity(
    memory: &ComputeMemory,
    graph: &Undirected,
    labels: &[i64],
    resolution: f64,
) -> Result<f64, CommunityError> {
    if labels.len() as u64 != graph.id_space() {
        return Err(ComputeError::IdSpaceMismatch {
            expected: graph.id_space(),
            found: labels.len() as u64,
        }
        .into());
    }
    let total = graph.total();
    if total == 0.0 {
        return Ok(0.0);
    }
    // The grouping's order and index, then each community's two sums.
    let _grouping = memory.charge((labels.len() as u64).saturating_mul(GROUPING_BYTES_PER_NODE))?;
    let communities = Communities::of(labels)?;
    let mut inside = zeroed(communities.count)?;
    let mut degrees = zeroed(communities.count)?;
    for node in 0..graph.id_space() {
        let community = communities.index[node as usize];
        let label = labels[node as usize];
        degrees[community] += graph.degree(node);
        inside[community] += graph
            .row(node)
            .iter()
            .filter(|&&(neighbour, _)| labels[neighbour as usize] == label)
            .map(|&(_, weight)| weight)
            .sum::<f64>();
    }
    Ok(inside
        .iter()
        .zip(&degrees)
        .map(|(&inside, &degree)| inside / total - resolution * (degree / total).powi(2))
        .sum())
}

/// `len` zeros, allocated with `try_reserve` (the grouping's charge covers
/// them: at most one community per node).
fn zeroed(len: usize) -> Result<Vec<f64>, CommunityError> {
    let mut zeros = Vec::new();
    zeros
        .try_reserve_exact(len)
        .map_err(|_| ComputeError::OutOfMemory { bytes: (len * 8) as u64 })?;
    zeros.resize(len, 0.0);
    Ok(zeros)
}

/// Each node's community as an index `0..count`, communities in ascending
/// label order.
pub(super) struct Communities {
    /// Each node's community.
    pub(super) index: Vec<usize>,
    /// The number of distinct labels.
    pub(super) count: usize,
}

impl Communities {
    /// The communities of `labels`.
    pub(super) fn of(labels: &[i64]) -> Result<Self, CommunityError> {
        let mut order: Vec<DenseId> = Vec::new();
        order
            .try_reserve_exact(labels.len())
            .map_err(|_| ComputeError::OutOfMemory { bytes: (labels.len() * 8) as u64 })?;
        order.extend(0..labels.len() as DenseId);
        order.sort_unstable_by_key(|&node| (labels[node as usize], node));
        let mut index = Vec::new();
        index
            .try_reserve_exact(labels.len())
            .map_err(|_| ComputeError::OutOfMemory { bytes: (labels.len() * 8) as u64 })?;
        index.resize(labels.len(), 0);
        let mut count = 0;
        for (at, &node) in order.iter().enumerate() {
            if at > 0 && labels[order[at - 1] as usize] != labels[node as usize] {
                count += 1;
            }
            index[node as usize] = count;
        }
        Ok(Self { index, count: if labels.is_empty() { 0 } else { count + 1 } })
    }
}

// Not under Miri: the tests build their graphs with the compute reference,
// which is not compiled under Miri.
#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;
    use crate::compute::reference::{self, Edges};
    use crate::interrupts::Cancellation;

    fn undirected(n: u64, edges: &Edges) -> Undirected {
        let mut both = edges.clone();
        both.extend(edges.iter().filter(|e| e.1 != e.2).map(|&(r, s, t, w)| (r, t, s, w)));
        let graph = reference::graph(n, 1, &both, true, false);
        Undirected::new(&ComputeMemory::new(u64::MAX), &Cancellation::new(), &graph, true, false)
            .unwrap()
    }

    #[test]
    fn a_self_loop_counts_once_in_its_nodes_degree() {
        // A self-loop of weight 2 on 0 and an edge 0 – 1 of weight 1:
        // k = (3, 1), 2m = 4; {0}, {1} scores 2/4 − (3/4)² − (1/4)² and
        // {0, 1} scores 0. (NetworkX counts the loop twice: −1/18.)
        let graph = undirected(2, &vec![(0, 0, 0, 2.0), (0, 0, 1, 1.0)]);
        let split = modularity(&ComputeMemory::new(u64::MAX), &graph, &[0, 1], 1.0).unwrap();
        assert!((split - (-0.125)).abs() < 1e-15, "{split}");
        assert_eq!(modularity(&ComputeMemory::new(u64::MAX), &graph, &[5, 5], 1.0).unwrap(), 0.0);
    }

    #[test]
    fn two_triangles_joined_by_an_edge() {
        let mut edges: Edges = vec![(0, 0, 1, 1.0), (0, 1, 2, 1.0), (0, 0, 2, 1.0)];
        edges.extend([(0, 3, 4, 1.0), (0, 4, 5, 1.0), (0, 3, 5, 1.0), (0, 2, 3, 1.0)]);
        let graph = undirected(6, &edges);
        // m = 7; each triangle: L = 3, degrees 7: 2 · (3/7 − (7/14)²).
        let q =
            modularity(&ComputeMemory::new(u64::MAX), &graph, &[1, 1, 1, 2, 2, 2], 1.0).unwrap();
        assert!((q - 2.0 * (3.0 / 7.0 - 0.25)).abs() < 1e-15, "{q}");
        let lower =
            modularity(&ComputeMemory::new(u64::MAX), &graph, &[1, 1, 1, 2, 2, 2], 0.5).unwrap();
        assert!((lower - 2.0 * (3.0 / 7.0 - 0.125)).abs() < 1e-15, "{lower}");
    }

    #[test]
    fn a_graph_without_edges_scores_zero() {
        assert_eq!(
            modularity(&ComputeMemory::new(u64::MAX), &undirected(3, &vec![]), &[0, 1, 2], 1.0)
                .unwrap(),
            0.0
        );
    }

    #[test]
    fn labels_cover_the_id_space() {
        assert!(matches!(
            modularity(&ComputeMemory::new(u64::MAX), &undirected(3, &vec![]), &[0], 1.0),
            Err(CommunityError::Compute(ComputeError::IdSpaceMismatch { .. }))
        ));
    }
}
