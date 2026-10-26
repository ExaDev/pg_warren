//! The degree-ordered orientation of the undirected simple graph
//! underlying the selected relations, which `triangles` counts over
//! (DESIGN.md §6.7). Nodes are ranked by (degree, dense id) and renamed by
//! rank; each edge is kept once, in the row of its lower-ranked endpoint,
//! self-loops dropped, rows sorted and their repeats dropped, in a CSR
//! charged to the compute budget. A row holds only neighbours of no lower
//! degree, so a hub's row is short (no row is longer than about `√(2M)`),
//! and each triangle is in exactly one pair of rows: its lowest-ranked
//! node's and its middle one's. Renaming by rank puts the hubs, which most
//! rows hold, at neighbouring ranks, so a search touches fewer cache lines.

use std::mem::size_of;

use super::super::symmetric::{each_edge, sort_rows};
use super::super::{ComputeError, ComputeMemory, MemoryCharge};
use crate::graph::{CsrRows, DenseId};
use crate::interrupts::{Cancellation, Cancelled};

/// How often the sequential passes over nodes check the cancellation flag,
/// in nodes.
const NODES_PER_CHECK: usize = 1 << 16;

/// Each node's neighbours of higher rank, sorted, each once, all named by
/// rank.
pub(super) struct Oriented {
    /// Each dense id's rank.
    ranks: Vec<u64>,
    /// `id_space + 1` positions into `targets`, by rank.
    offsets: Vec<u64>,
    targets: Vec<u64>,
    _charges: [MemoryCharge; 3],
}

impl Oriented {
    /// The most bytes [`Self::new`] charges over `forward`: each node's
    /// rank, the offsets, and one entry per stored edge (self-loops and
    /// repeats, which it drops, included).
    pub(super) fn bytes(forward: &[CsrRows<'_>], id_space: u64) -> u64 {
        let edges = forward.iter().map(CsrRows::stored_edges).fold(0, u64::saturating_add);
        let ranks = id_space.saturating_mul(size_of::<u64>() as u64);
        let offsets = ranks.saturating_add(size_of::<u64>() as u64);
        let entries = edges.saturating_mul(size_of::<u64>() as u64);
        ranks.saturating_add(offsets).saturating_add(entries)
    }

    pub(super) fn new(
        memory: &ComputeMemory,
        cancellation: &Cancellation,
        forward: &[CsrRows<'_>],
        id_space: u64,
    ) -> Result<Self, ComputeError> {
        let nodes = id_space as usize;
        let (mut ranks, ranks_charge) = memory.filled(nodes, 0u64)?;
        each_edge(cancellation, forward, id_space, &mut |u, v| {
            ranks[u as usize] += 1;
            ranks[v as usize] += 1;
        })?;
        let (mut offsets, offsets_charge) = memory.filled(nodes + 1, 0u64)?;
        rank(&mut ranks, &mut offsets, cancellation)?;
        offsets.fill(0);
        // An edge's row (its lower rank) and entry (its higher).
        let oriented = |u: DenseId, v: DenseId| {
            let (u, v) = (ranks[u as usize], ranks[v as usize]);
            (u.min(v) as usize, u.max(v))
        };
        each_edge(cancellation, forward, id_space, &mut |u, v| offsets[oriented(u, v).0] += 1)?;
        // Running ends; placing each entry then moves a row's end to its
        // start.
        let mut end = 0;
        for offset in &mut offsets[..nodes] {
            end += *offset;
            *offset = end;
        }
        offsets[nodes] = end;
        let (mut targets, targets_charge) = memory.filled(end as usize, 0u64)?;
        each_edge(cancellation, forward, id_space, &mut |u, v| {
            let (row, target) = oriented(u, v);
            offsets[row] -= 1;
            targets[offsets[row] as usize] = target;
        })?;
        sort_rows(&offsets, &mut targets, cancellation)?;
        drop_repeats(&mut offsets, &mut targets, cancellation)?;
        let _charges = [ranks_charge, offsets_charge, targets_charge];
        Ok(Self { ranks, offsets, targets, _charges })
    }

    /// Each dense id's rank.
    pub(super) fn ranks(&self) -> &[u64] {
        &self.ranks
    }

    /// The row of the node of rank `rank`.
    pub(super) fn row(&self, rank: usize) -> &[u64] {
        &self.targets[self.offsets[rank] as usize..self.offsets[rank + 1] as usize]
    }

    /// The number of nodes.
    pub(super) fn nodes(&self) -> usize {
        self.offsets.len() - 1
    }
}

/// Checks the cancellation flag at every [`NODES_PER_CHECK`]th node.
fn check(cancellation: &Cancellation, node: usize) -> Result<(), Cancelled> {
    match node.is_multiple_of(NODES_PER_CHECK) && cancellation.is_cancelled() {
        true => Err(Cancelled),
        false => Ok(()),
    }
}

/// Turns each node's degree into its rank: its place in the order of
/// (degree, dense id). A counting sort over `buckets` (`nodes + 1` zeros,
/// left dirty): a degree past the number of nodes (only repeated entries
/// take it there) counts as that number, so it ranks in O(N).
fn rank(
    degrees: &mut [u64],
    buckets: &mut [u64],
    cancellation: &Cancellation,
) -> Result<(), Cancelled> {
    let nodes = degrees.len();
    for (node, degree) in degrees.iter_mut().enumerate() {
        check(cancellation, node)?;
        *degree = (*degree).min(nodes as u64);
        buckets[*degree as usize] += 1;
    }
    // Each degree's first rank.
    let mut first = 0;
    for bucket in buckets.iter_mut() {
        (*bucket, first) = (first, first + *bucket);
    }
    for (node, degree) in degrees.iter_mut().enumerate() {
        check(cancellation, node)?;
        let next = &mut buckets[*degree as usize];
        (*degree, *next) = (*next, *next + 1);
    }
    Ok(())
}

/// Keeps each sorted row's distinct entries, moved to the front of
/// `targets` (an edge in several relations, or in both directions, was
/// placed once for each).
fn drop_repeats(
    offsets: &mut [u64],
    targets: &mut Vec<u64>,
    cancellation: &Cancellation,
) -> Result<(), Cancelled> {
    let nodes = offsets.len() - 1;
    let (mut kept, mut start) = (0, 0);
    for node in 0..nodes {
        check(cancellation, node)?;
        let (end, first) = (offsets[node + 1] as usize, kept);
        offsets[node] = kept as u64;
        for at in start..end {
            if kept == first || targets[kept - 1] != targets[at] {
                targets[kept] = targets[at];
                kept += 1;
            }
        }
        start = end;
    }
    offsets[nodes] = kept as u64;
    targets.truncate(kept);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_rank_by_degree_then_dense_id() {
        let cancellation = Cancellation::new();
        // Degree 7 is past the 5 nodes, so it ties with 5.
        let mut degrees = vec![2, 0, 5, 2, 7];
        let mut buckets = vec![0; 6];
        rank(&mut degrees, &mut buckets, &cancellation).unwrap();
        assert_eq!(degrees, vec![1, 0, 3, 2, 4]);
    }
}
