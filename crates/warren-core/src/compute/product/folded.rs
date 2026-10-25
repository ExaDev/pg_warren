//! Rows in which each target appears once, its entries' weights folded:
//! how a cost reading holds a pair linked in several relations (or both
//! ways, undirected), as its lightest or heaviest edge (DESIGN.md §5.3).
//!
//! The rows are built once, from edges given twice in the same order (a
//! count, then a fill), in O(N + M) and a sort of each row, so a kernel
//! that reads every row, or reads rows many times, pays for several
//! relations' rows once rather than on every read.

use rayon::prelude::*;

use super::super::{ComputeError, ComputeMemory, MemoryCharge};
use crate::graph::DenseId;
use crate::interrupts::{Cancellation, Cancelled, Work};

/// How a pair's weights fold into one: called with the weight kept so far
/// and the next, in the order the edges were given.
pub(crate) type Fold = fn(f64, f64) -> f64;

/// The nodes one sort task handles; a task checks the cancellation flag
/// first. Small under test, so tests cross task boundaries.
const NODES_PER_TASK: usize = if cfg!(test) { 3 } else { 4096 };

/// Each node's targets, ascending, each once with its edges' weights
/// folded.
#[derive(Debug)]
pub(crate) struct FoldedRows {
    /// `id_space + 1` positions into `entries`.
    offsets: Vec<u64>,
    entries: Vec<(DenseId, f64)>,
    _charges: [MemoryCharge; 2],
}

impl FoldedRows {
    /// The bytes [`Self::new`] charges for `edges` edges over `id_space`
    /// nodes.
    pub(crate) fn bytes(id_space: u64, edges: u64) -> u64 {
        let entry = size_of::<(DenseId, f64)>() as u64;
        (id_space + 1).saturating_mul(8).saturating_add(edges.saturating_mul(entry))
    }

    /// The rows of the edges `each_edge` gives, `(source, target, weight)`
    /// to the callback it is passed: it is called twice and must give the
    /// same edges in the same order both times. A target's weights are
    /// folded by `fold` in that order.
    pub(crate) fn new(
        memory: &ComputeMemory,
        cancellation: &Cancellation,
        id_space: u64,
        mut each_edge: impl FnMut(&mut dyn FnMut(DenseId, DenseId, f64)) -> Result<(), ComputeError>,
        fold: Fold,
    ) -> Result<Self, ComputeError> {
        let (mut offsets, offsets_charge) = memory.filled(id_space as usize + 1, 0u64)?;
        each_edge(&mut |source, _, _| offsets[source as usize] += 1)?;
        let mut end = 0;
        for offset in &mut offsets[..id_space as usize] {
            end += *offset;
            *offset = end;
        }
        offsets[id_space as usize] = end;
        let (mut entries, entries_charge) = memory.filled(end as usize, (0 as DenseId, 0.0))?;
        // Filled from each row's end, so a row holds its edges reversed.
        each_edge(&mut |source, target, weight| {
            let offset = &mut offsets[source as usize];
            *offset -= 1;
            entries[*offset as usize] = (target, weight);
        })?;
        let mut rows = Self { offsets, entries, _charges: [offsets_charge, entries_charge] };
        rows.fold(cancellation, fold)?;
        Ok(rows)
    }

    /// The row of `node`.
    pub(crate) fn row(&self, node: DenseId) -> &[(DenseId, f64)] {
        let node = node as usize;
        &self.entries[self.offsets[node] as usize..self.offsets[node + 1] as usize]
    }

    /// Sorts every row by target (stably, so a target's entries stay in
    /// reverse order) and folds each target's weights into its first entry
    /// in the order they were given, marking the rest with
    /// [`DenseId::MAX`] so the row keeps its length until
    /// [`Self::compact`] drops them.
    fn fold(&mut self, cancellation: &Cancellation, fold: Fold) -> Result<(), Cancelled> {
        let offsets = &self.offsets;
        let nodes = offsets.len() - 1;
        let mut tasks = Vec::new();
        let mut rest = self.entries.as_mut_slice();
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
                let row = &mut chunk[row];
                row.sort_by_key(|&(target, _)| target);
                for run in row.chunk_by_mut(|a, b| a.0 == b.0) {
                    let folded = run.iter().rev().map(|&(_, weight)| weight).reduce(fold);
                    if let (Some(first), Some(weight)) = (run.first_mut(), folded) {
                        first.1 = weight;
                    }
                    run.iter_mut().skip(1).for_each(|entry| entry.0 = DenseId::MAX);
                }
            }
            Ok(())
        })?;
        self.compact(cancellation)
    }

    /// Drops the entries [`Self::fold`] marked, moving rows down.
    fn compact(&mut self, cancellation: &Cancellation) -> Result<(), Cancelled> {
        let mut work = Work::new(cancellation);
        let mut kept = 0;
        let mut start = 0;
        for node in 0..self.offsets.len() - 1 {
            let end = self.offsets[node + 1] as usize;
            self.offsets[node] = kept as u64;
            for i in start..end {
                work.spend(1)?;
                if self.entries[i].0 != DenseId::MAX {
                    self.entries[kept] = self.entries[i];
                    kept += 1;
                }
            }
            start = end;
        }
        let nodes = self.offsets.len() - 1;
        self.offsets[nodes] = kept as u64;
        self.entries.truncate(kept);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folded(edges: &[(DenseId, DenseId, f64)], fold: Fold) -> FoldedRows {
        let memory = ComputeMemory::new(1 << 20);
        let each_edge = |edge: &mut dyn FnMut(DenseId, DenseId, f64)| {
            edges.iter().for_each(|&(source, target, weight)| edge(source, target, weight));
            Ok(())
        };
        FoldedRows::new(&memory, &Cancellation::new(), 4, each_edge, fold).unwrap()
    }

    #[test]
    fn each_target_is_once_its_weights_folded_in_the_order_given() {
        let edges = [(0, 2, 5.0), (0, 1, 1.0), (0, 2, 3.0), (2, 0, 4.0), (0, 2, 7.0)];
        let lightest = folded(&edges, f64::min);
        assert_eq!(lightest.row(0), [(1, 1.0), (2, 3.0)]);
        assert_eq!(lightest.row(1), []);
        assert_eq!(lightest.row(2), [(0, 4.0)]);
        assert_eq!(lightest.row(3), []);
        assert_eq!(folded(&edges, f64::max).row(0), [(1, 1.0), (2, 7.0)]);
        // The first weight given is kept, the last replaces it.
        assert_eq!(folded(&edges, |kept, _| kept).row(0), [(1, 1.0), (2, 5.0)]);
        assert_eq!(folded(&edges, |_, next| next).row(0), [(1, 1.0), (2, 7.0)]);
    }

    #[test]
    fn the_rows_are_charged_until_dropped() {
        let memory = ComputeMemory::new(1 << 20);
        let each_edge = |edge: &mut dyn FnMut(DenseId, DenseId, f64)| {
            [(0, 1, 1.0), (0, 1, 2.0)].iter().for_each(|&(s, t, w)| edge(s, t, w));
            Ok(())
        };
        let rows = FoldedRows::new(&memory, &Cancellation::new(), 2, each_edge, f64::min).unwrap();
        assert_eq!(memory.in_use(), FoldedRows::bytes(2, 2));
        drop(rows);
        assert_eq!(memory.in_use(), 0);
        let tight = ComputeMemory::new(FoldedRows::bytes(2, 2) - 1);
        let refused = FoldedRows::new(&tight, &Cancellation::new(), 2, each_edge, f64::min);
        assert!(matches!(refused, Err(ComputeError::MemoryLimit { .. })));
    }

    #[test]
    fn a_cancellation_stops_the_fold() {
        let memory = ComputeMemory::new(1 << 20);
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let each_edge = |edge: &mut dyn FnMut(DenseId, DenseId, f64)| {
            edge(0, 1, 1.0);
            Ok(())
        };
        let found = FoldedRows::new(&memory, &cancellation, 2, each_edge, f64::min);
        assert_eq!(found.map(|_| ()), Err(ComputeError::Cancelled));
    }
}
