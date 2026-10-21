//! The symmetric adjacency of the undirected simple graph underlying the
//! selected relations (DESIGN.md §6.7): each node's neighbours in either
//! direction, self-loops dropped, rows sorted, in a CSR charged to the
//! compute budget. `triangles` and `core_number` read it; no reverse CSR
//! is needed to build it.

use std::cmp::Ordering;
use std::mem::size_of;

use rayon::prelude::*;

use super::{ComputeError, ComputeMemory, MemoryCharge};
use crate::graph::{CsrRows, DenseId, GraphCsr, RelationSelection};
use crate::interrupts::{Cancellation, Cancelled, Work};

/// The nodes one sort task handles; a task checks the cancellation flag
/// first. Small under test, so tests cross task boundaries.
const NODES_PER_TASK: usize = if cfg!(test) { 3 } else { 4096 };

/// How often the sequential build checks the cancellation flag, in nodes.
const NODES_PER_CHECK: u64 = 1 << 16;

/// The forward rows of the selected relations: every relation is read
/// through its all-relations rows (stored or merged on the fly), so an
/// edge in several relations is one entry and the scratch charged does not
/// depend on whether the merged CSR is stored; a selection through each
/// relation's CSR.
pub(super) fn selected_rows<'g>(
    graph: &'g GraphCsr,
    relations: RelationSelection<'_>,
) -> Result<Vec<CsrRows<'g>>, ComputeError> {
    Ok(match relations {
        RelationSelection::All => vec![graph.all_relations().forward()],
        RelationSelection::Only(_) => graph
            .selected(relations)
            .map_err(|relation| ComputeError::UnknownRelation { relation: relation.index() })?
            .into_iter()
            .map(|relation| relation.forward().into())
            .collect(),
    })
}

/// Each node's neighbours in either direction (self-loops dropped), each
/// row sorted; a neighbour in several relations or both directions
/// repeats.
pub(super) struct Symmetric {
    /// `id_space + 1` positions into `targets`.
    offsets: Vec<u64>,
    targets: Vec<DenseId>,
    _charges: [MemoryCharge; 2],
}

impl Symmetric {
    /// At most the bytes [`Self::new`] charges over `forward`: the offsets,
    /// and both directions of every stored edge (self-loops, which it
    /// drops, included).
    pub(super) fn bytes(forward: &[CsrRows<'_>], id_space: u64) -> u64 {
        let edges = forward.iter().map(CsrRows::stored_edges).fold(0, u64::saturating_add);
        let offsets = id_space.saturating_add(1).saturating_mul(size_of::<u64>() as u64);
        offsets.saturating_add(edges.saturating_mul(2 * size_of::<DenseId>() as u64))
    }

    pub(super) fn new(
        memory: &ComputeMemory,
        cancellation: &Cancellation,
        forward: &[CsrRows<'_>],
        id_space: u64,
    ) -> Result<Self, ComputeError> {
        let each_edge = |f: &mut dyn FnMut(DenseId, DenseId)| -> Result<(), Cancelled> {
            for u in 0..id_space {
                if u % NODES_PER_CHECK == 0 && cancellation.is_cancelled() {
                    return Err(Cancelled);
                }
                for csr in forward {
                    csr.neighbours(u).filter(|&v| v != u).for_each(|v| f(u, v));
                }
            }
            Ok(())
        };
        let (mut offsets, offsets_charge) = memory.filled(id_space as usize + 1, 0u64)?;
        each_edge(&mut |u, v| {
            offsets[u as usize] += 1;
            offsets[v as usize] += 1;
        })?;
        // Running ends; placing each entry then moves a row's end to its
        // start.
        let mut end = 0;
        for offset in &mut offsets[..id_space as usize] {
            end += *offset;
            *offset = end;
        }
        offsets[id_space as usize] = end;
        let (mut targets, targets_charge) = memory.filled(end as usize, 0 as DenseId)?;
        each_edge(&mut |u, v| {
            for (row, target) in [(u, v), (v, u)] {
                let offset = &mut offsets[row as usize];
                *offset -= 1;
                targets[*offset as usize] = target;
            }
        })?;
        let mut symmetric = Self { offsets, targets, _charges: [offsets_charge, targets_charge] };
        symmetric.sort(cancellation)?;
        Ok(symmetric)
    }

    /// The row of `node`.
    pub(super) fn row(&self, node: usize) -> &[DenseId] {
        &self.targets[self.offsets[node] as usize..self.offsets[node + 1] as usize]
    }

    /// Sorts every row, in parallel tasks of whole rows.
    fn sort(&mut self, cancellation: &Cancellation) -> Result<(), Cancelled> {
        let offsets = &self.offsets;
        let nodes = offsets.len() - 1;
        let mut tasks = Vec::new();
        let mut rest = self.targets.as_mut_slice();
        for first in (0..nodes).step_by(NODES_PER_TASK) {
            let last = nodes.min(first + NODES_PER_TASK);
            let len = (offsets[last] - offsets[first]) as usize;
            let (chunk, tail) = std::mem::take(&mut rest).split_at_mut(len);
            tasks.push((first..last, chunk));
            rest = tail;
        }
        tasks.into_par_iter().try_for_each(|(nodes, chunk)| {
            if cancellation.is_cancelled() {
                return Err(Cancelled);
            }
            let base = offsets[nodes.start];
            for node in nodes {
                let row = (offsets[node] - base) as usize..(offsets[node + 1] - base) as usize;
                chunk[row].sort_unstable();
            }
            Ok(())
        })
    }
}

/// The distinct values of a sorted row.
pub(super) fn distinct(row: &[DenseId]) -> impl Iterator<Item = DenseId> + '_ {
    row.iter().enumerate().filter(|&(i, v)| i == 0 || row[i - 1] != *v).map(|(_, &v)| v)
}

/// How much longer one row must be than the other before the shorter is
/// searched into the longer instead of merged with it.
const SKEW: usize = 16;

/// The number of distinct values two sorted rows share, calling `each`
/// with each in ascending order: a merge when the rows are of similar
/// length, else a binary search of each value of the shorter in the rest
/// of the longer, so a hub's row costs about its neighbour's length, not
/// its own. Its work is spent on `work`.
pub(super) fn shared(
    a: &[DenseId],
    b: &[DenseId],
    work: &mut Work<'_>,
    mut each: impl FnMut(DenseId),
) -> Result<u64, Cancelled> {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if short.len().saturating_mul(SKEW) < long.len() {
        let mut rest = long;
        let mut shared = 0;
        for value in distinct(short) {
            let at = rest.partition_point(|&x| x < value);
            if rest.get(at) == Some(&value) {
                shared += 1;
                each(value);
            }
            rest = &rest[at..];
            work.spend(1 + (usize::BITS - long.len().leading_zeros()) as u64)?;
        }
        return Ok(shared);
    }
    let (mut i, mut j, mut shared) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                let value = a[i];
                shared += 1;
                each(value);
                while a.get(i) == Some(&value) {
                    i += 1;
                }
                while b.get(j) == Some(&value) {
                    j += 1;
                }
            }
        }
    }
    work.spend((a.len() + b.len()) as u64)?;
    Ok(shared)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn intersections_of_skewed_rows_count_distinct_shared_values(
            mut a in prop::collection::vec(0u64..400, 0..300),
            mut b in prop::collection::vec(0u64..400, 0..12),
        ) {
            a.sort_unstable();
            b.sort_unstable();
            let expected = a.iter().collect::<BTreeSet<_>>().intersection(&b.iter().collect()).count() as u64;
            let cancellation = Cancellation::new();
            let mut work = Work::new(&cancellation);
            prop_assert_eq!(shared(&a, &b, &mut work, |_| ()), Ok(expected));
            prop_assert_eq!(shared(&b, &a, &mut work, |_| ()), Ok(expected));
        }
    }

    #[test]
    fn shared_values_are_given_in_ascending_order() {
        let cancellation = Cancellation::new();
        let mut work = Work::new(&cancellation);
        let long: Vec<DenseId> = (0..400).collect();
        for (a, b) in [(&[1, 1, 3, 9][..], &[0, 1, 3, 3, 4, 9][..]), (&[2, 7, 399], &long[..])] {
            let mut found = Vec::new();
            shared(a, b, &mut work, |value| found.push(value)).unwrap();
            let expected: Vec<DenseId> = a
                .iter()
                .copied()
                .filter(|v| b.contains(v))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            assert_eq!(found, expected);
        }
    }
}
