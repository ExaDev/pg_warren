//! Counting a pair in several relations once (DESIGN.md §6.6): which of
//! the parent's stored entries count, each pair in the first relation
//! that holds it.

use crate::compute::ComputeError;
use crate::graph::{AnyCsr, BuildBuffer, BuildMemory, DenseId, RelationIndex};
use crate::interrupts::Work;

use super::CommunityError;

/// For each stored entry of each of the parent's relations' CSRs, whether
/// it counts: an entry `u → v` counts in the first relation, in relation
/// order, that holds `u → v`, and in no later one.
pub(super) struct FirstEntries {
    /// Where each relation's entries start in `counts`.
    starts: BuildBuffer<usize>,
    counts: BuildBuffer<bool>,
}

impl FirstEntries {
    /// Marks the entries of `relations`' CSRs over `id_space` nodes,
    /// charged to `memory`: one sweep of the rows in ascending order, each
    /// relation's row of a node sought from where the last one left off,
    /// remembering the last source that reached each target. O(N·R + M)
    /// for N nodes, R relations and M entries.
    pub(super) fn mark(
        relations: &[(RelationIndex, &AnyCsr)],
        id_space: u64,
        memory: &BuildMemory,
        work: &mut Work<'_>,
    ) -> Result<Self, CommunityError> {
        let mut starts = BuildBuffer::with_capacity(memory, relations.len() + 1)?;
        let mut total = 0usize;
        starts.push(total)?;
        for (_, csr) in relations {
            total += csr.edge_count() as usize;
            starts.push(total)?;
        }
        let mut counts = BuildBuffer::filled(memory, total, false)?;
        let nodes =
            usize::try_from(id_space).map_err(|_| ComputeError::OutOfMemory { bytes: u64::MAX })?;
        let mut reached_from = BuildBuffer::filled(memory, nodes, DenseId::MAX)?;
        let mut hints = BuildBuffer::filled(memory, relations.len(), 0usize)?;
        let slices: Vec<_> = relations.iter().map(|(_, csr)| csr.slices()).collect();
        for source in 0..id_space {
            for (position, csr) in slices.iter().enumerate() {
                let entries = csr.seek(source, &mut hints[position]);
                work.spend(entries.len() as u64 + 1).map_err(ComputeError::from)?;
                let start = starts[position];
                for (at, target) in entries.clone().zip(csr.neighbours_at(entries)) {
                    // A target past `id_space` is in no community, and
                    // `contract` passes over its entries without asking.
                    let Some(reached) = reached_from.get_mut(target as usize) else { continue };
                    if *reached != source {
                        *reached = source;
                        counts[start + at] = true;
                    }
                }
            }
        }
        Ok(Self { starts, counts })
    }

    /// Whether entry `at` (its position in the CSR's targets) of the
    /// relation at `position` counts.
    pub(super) fn counts(&self, position: usize, at: usize) -> bool {
        self.counts[self.starts[position] + at]
    }
}

// Not under Miri: the reference graphs (`compute::reference`) are not built
// there.
#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;
    use crate::compute::reference::{self, Edges};
    use crate::interrupts::Cancellation;

    #[test]
    fn a_pair_counts_in_the_first_relation_that_holds_it() {
        // 0 → 1 in relations 0 and 2, 0 → 2 in relation 1 only, 3 → 1 in
        // relations 1 and 2.
        let edges: Edges =
            vec![(0, 0, 1, 1.0), (2, 0, 1, 1.0), (1, 0, 2, 1.0), (1, 3, 1, 1.0), (2, 3, 1, 1.0)];
        let graph = reference::graph(4, 3, &edges, false, false);
        let indexes = reference::relations(3);
        let relations: Vec<_> =
            indexes.iter().zip(graph.relations()).map(|(&r, csr)| (r, csr.forward())).collect();
        let cancellation = Cancellation::new();
        let mut work = Work::new(&cancellation);
        let first =
            FirstEntries::mark(&relations, 4, &BuildMemory::unlimited(), &mut work).unwrap();
        // Relation 0 holds 0 → 1; relation 1 holds 0 → 2 and 3 → 1;
        // relation 2 holds 0 → 1 and 3 → 1, both in an earlier relation.
        assert!(first.counts(0, 0));
        assert!(first.counts(1, 0) && first.counts(1, 1));
        assert!(!first.counts(2, 0) && !first.counts(2, 1));
    }
}
