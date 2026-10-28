//! Contracting the view (DESIGN.md §6.6, §6.7): the `B` of the quotient
//! by a membership, `PᵀBP`, from `B` itself, without building the
//! quotient as a graph version. `B` is closed under contraction, so it is
//! the view of the quotient `warren.contract` builds, and a quotient's
//! own quotient is the original's by the composed membership: Leiden
//! contracts each level's view into the next.

use std::mem::size_of;

use super::Undirected;
use crate::community::CommunityError;
use crate::compute::{ComputeError, ComputeMemory, MemoryCharge};
use crate::graph::{DenseId, Membership};
use crate::interrupts::{Interrupts, Work};

impl Undirected {
    /// `PᵀBP`, the view of the quotient by `membership` (one entry per
    /// node of this view; a node in no community must have no entries, as
    /// a tombstoned node has none): its entry between communities `c` and
    /// `d` sums the entries between their members, a community's
    /// self-loop the entries among its members, so every community's
    /// degree is the sum of its members' and the total is unchanged.
    /// Gustavson's row-by-row product, as `contract`'s: each community's
    /// members in ascending order, each row in order, summed into a dense
    /// row (an entry's first weight taken as it is), whose touched entries
    /// make the community's row in ascending order. On the calling thread,
    /// polling `interrupts` first and every 65,536 row entries; its scratch (40 B a
    /// community and 8 B a node) and the quotient's rows (at most this
    /// view's entries, until they are known) are charged to `memory`.
    pub fn contract(
        &self,
        memory: &ComputeMemory,
        interrupts: &mut dyn Interrupts,
        membership: &Membership,
    ) -> Result<Self, CommunityError> {
        if membership.id_space() != self.id_space() {
            return Err(ComputeError::IdSpaceMismatch {
                expected: self.id_space(),
                found: membership.id_space(),
            }
            .into());
        }
        let mut work = Work::polling(interrupts);
        work.check()?;
        let communities = usize::try_from(membership.quotient_id_space())
            .map_err(|_| ComputeError::OutOfMemory { bytes: u64::MAX })?;
        let grouped = Grouped::new(memory, membership, &mut work)?;
        let (mut offsets, offsets_charge) = memory.filled(communities + 1, 0u64)?;
        let bound = self.entries.len();
        let mut entries_charge = memory.charge_elements::<(DenseId, f64)>(bound)?;
        let mut entries: Vec<(DenseId, f64)> = Vec::new();
        entries
            .try_reserve_exact(bound)
            .map_err(|_| ComputeError::OutOfMemory { bytes: entries_charge.bytes() })?;
        let (mut sums, _sums_charge) = memory.filled(communities, 0.0f64)?;
        let (mut row_of, _row_of_charge) = memory.filled(communities, usize::MAX)?;
        let (mut touched, _touched_charge) = memory.filled(communities, 0 as DenseId)?;
        for community in 0..communities {
            let mut len = 0;
            for &member in grouped.of(community) {
                let row = self.row(member);
                work.spend(row.len() as u64 + 1)?;
                for &(neighbour, weight) in row {
                    let Some(other) = membership.community(neighbour) else { continue };
                    let at = other as usize;
                    if row_of[at] == community {
                        sums[at] += weight;
                    } else {
                        row_of[at] = community;
                        sums[at] = weight;
                        touched[len] = other;
                        len += 1;
                    }
                }
            }
            let row = &mut touched[..len];
            row.sort_unstable();
            entries.extend(row.iter().map(|&other| (other, sums[other as usize])));
            offsets[community + 1] = entries.len() as u64;
        }
        let unused = (bound - entries.len()) * size_of::<(DenseId, f64)>();
        entries.shrink_to_fit();
        entries_charge.shrink(unused as u64);
        Self::from_rows(memory, (offsets, offsets_charge), (entries, entries_charge))
    }
}

/// The nodes of each community of a membership, ascending,
/// counting-sorted: where each community's members start (one more entry
/// than there are communities), and the members.
struct Grouped {
    starts: Vec<u64>,
    members: Vec<DenseId>,
    _charges: [MemoryCharge; 2],
}

impl Grouped {
    fn new(
        memory: &ComputeMemory,
        membership: &Membership,
        work: &mut Work<'_>,
    ) -> Result<Self, CommunityError> {
        let communities = membership.quotient_id_space() as usize;
        let (mut starts, starts_charge) = memory.filled(communities + 1, 0u64)?;
        for node in 0..membership.id_space() {
            work.spend(1)?;
            if let Some(community) = membership.community(node) {
                starts[community as usize + 1] += 1;
            }
        }
        for community in 0..communities {
            starts[community + 1] += starts[community];
        }
        let (mut next, _next_charge) = memory.filled(communities, 0u64)?;
        next.copy_from_slice(&starts[..communities]);
        let members_len = starts[communities] as usize;
        let (mut members, members_charge) = memory.filled(members_len, 0 as DenseId)?;
        for node in 0..membership.id_space() {
            if let Some(community) = membership.community(node) {
                let at = &mut next[community as usize];
                members[*at as usize] = node;
                *at += 1;
            }
        }
        Ok(Self { starts, members, _charges: [starts_charge, members_charge] })
    }

    /// The members of `community`, ascending.
    fn of(&self, community: usize) -> &[DenseId] {
        &self.members[self.starts[community] as usize..self.starts[community + 1] as usize]
    }
}

// Not under Miri: the compute pool's work stealing runs through
// crossbeam-epoch, which Miri's Stacked Borrows model rejects.
#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::community::{contract, CommunityError, Contraction, Weighting};
    use crate::compute::reference::{self, Edges};
    use crate::compute::{ComputeError, ComputeMemory, Workers};
    use crate::graph::{BuildMemory, DenseId, GraphCsr, Membership};
    use crate::interrupts::{Cancellation, Uninterrupted};

    const WEIGHTED: Weighting = Weighting { weighted: true, per_relation: false };

    fn undirected(graph: &GraphCsr, weighting: Weighting, directed: bool) -> Undirected {
        let memory = ComputeMemory::new(u64::MAX);
        Undirected::new(&memory, &Cancellation::new(), graph, weighting, directed).unwrap()
    }

    fn membership(communities: &[Option<DenseId>], quotient_id_space: u64) -> Membership {
        Membership::new(communities, quotient_id_space).unwrap()
    }

    fn contracted(b: &Undirected, membership: &Membership) -> Undirected {
        let memory = ComputeMemory::new(u64::MAX);
        b.contract(&memory, &mut Uninterrupted, membership).unwrap()
    }

    fn assert_same(found: &Undirected, expected: &Undirected) {
        assert_eq!(found.id_space(), expected.id_space());
        for node in 0..expected.id_space() {
            assert_eq!(found.row(node), expected.row(node), "row {node}");
        }
        let bits = |b: &Undirected| b.degrees().iter().map(|d| d.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(found), bits(expected));
        assert_eq!(found.total().to_bits(), expected.total().to_bits());
    }

    #[test]
    fn contracting_sums_the_weights_between_communities_and_keeps_self_loops() {
        // Directed: 0 → 1 (inside 0), 1 → 2 and 2 → 0 (between), 2 → 2
        // (a self-loop), 3 is in no community, as a tombstoned node.
        let edges: Edges = vec![(0, 0, 1, 1.0), (0, 1, 2, 2.0), (0, 2, 0, 4.0), (0, 2, 2, 8.0)];
        let graph = reference::graph(4, 1, &edges, true, false);
        let b = undirected(&graph, WEIGHTED, true);
        let quotient = contracted(&b, &membership(&[Some(0), Some(0), Some(1), None], 2));
        assert_eq!(quotient.row(0), [(0, 2.0), (1, 6.0)]);
        assert_eq!(quotient.row(1), [(0, 6.0), (1, 16.0)]);
        assert_eq!((quotient.degree(0), quotient.degree(1)), (8.0, 22.0));
        assert_eq!(quotient.total(), b.total());
    }

    #[test]
    fn contracting_by_singletons_changes_nothing() {
        let edges: Edges = (0..30u64).map(|i| (0, i % 7, (i * 3) % 7, 0.1 * i as f64)).collect();
        let graph = reference::graph(7, 1, &edges, true, false);
        let b = undirected(&graph, WEIGHTED, true);
        let singletons: Vec<_> = (0..7).map(Some).collect();
        assert_same(&contracted(&b, &membership(&singletons, 7)), &b);
    }

    #[test]
    fn contracting_refuses_a_membership_of_another_id_space() {
        let graph = reference::graph(3, 1, &vec![(0, 0, 1, 1.0)], true, false);
        let b = undirected(&graph, WEIGHTED, true);
        let memory = ComputeMemory::new(u64::MAX);
        let short = membership(&[Some(0), Some(0)], 1);
        assert!(matches!(
            b.contract(&memory, &mut Uninterrupted, &short),
            Err(CommunityError::Compute(ComputeError::IdSpaceMismatch { expected: 3, found: 2 }))
        ));
    }

    #[test]
    fn contracting_is_charged_to_the_budget_and_stops_when_cancelled() {
        let edges: Edges = (0..40u64).map(|i| (0, i % 7, (i * 3) % 7, 1.0)).collect();
        let graph = reference::graph(7, 1, &edges, true, false);
        let b = undirected(&graph, WEIGHTED, true);
        let p = membership(&(0..7).map(|n| Some(n % 3)).collect::<Vec<_>>(), 3);
        let small = ComputeMemory::new(64);
        assert!(matches!(
            b.contract(&small, &mut Uninterrupted, &p),
            Err(CommunityError::Compute(ComputeError::MemoryLimit { .. }))
        ));
        assert_eq!(small.in_use(), 0);
        let memory = ComputeMemory::new(u64::MAX);
        let quotient = b.contract(&memory, &mut Uninterrupted, &p).unwrap();
        assert!(memory.in_use() > 0);
        drop(quotient);
        assert_eq!(memory.in_use(), 0);
        let mut cancelled = Cancellation::new();
        cancelled.cancel();
        assert!(matches!(
            b.contract(&memory, &mut cancelled, &p),
            Err(CommunityError::Compute(ComputeError::Cancelled))
        ));
    }

    /// Partitions of `n` nodes into at most 4 communities, a node in none
    /// with probability 1/10 (it then has no edges, as a tombstoned node).
    fn partitioned() -> impl Strategy<Value = (u64, Edges, Vec<Option<DenseId>>)> {
        (1u64..14).prop_flat_map(|n| {
            (
                Just(n),
                prop::collection::vec((0usize..3, 0..n, 0..n, 1..5u32), 0..50),
                prop::collection::vec(prop::option::weighted(0.9, 0u64..4), n as usize),
            )
                .prop_map(|(n, edges, communities)| {
                    let edges = edges
                        .into_iter()
                        .filter(|&(_, s, t, _)| {
                            communities[s as usize].is_some() && communities[t as usize].is_some()
                        })
                        .map(|(r, s, t, w)| (r, s, t, f64::from(w)))
                        .collect();
                    (n, edges, communities)
                })
        })
    }

    proptest! {
        /// Leiden's levels were the views of `contract`'s quotients, built
        /// as graph versions (the library's way through 0.7.0); contracting
        /// the view in memory gives the same view to the bit, with integer
        /// weights, in either counting of a pair in several relations,
        /// weighted or not, directed or not.
        #[test]
        fn contracting_the_view_is_the_view_of_the_quotient(
            (n, edges, communities) in partitioned(),
            weighted in any::<bool>(),
            per_relation in any::<bool>(),
            undirected_graph in any::<bool>(),
        ) {
            let mut stored = edges.clone();
            if undirected_graph {
                stored.extend(edges.iter().filter(|e| e.1 != e.2).map(|&(r, s, t, w)| (r, t, s, w)));
            }
            let graph = reference::graph(n, 3, &stored, true, false);
            let p = membership(&communities, 4);
            let contraction = Contraction {
                weighted,
                per_relation,
                drop_self_loops: false,
                undirected: undirected_graph,
            };
            let indexes = reference::relations(3);
            let relations: Vec<_> =
                indexes.iter().zip(graph.relations()).map(|(&r, csr)| (r, csr.forward())).collect();
            let workers = Workers::new(2).unwrap();
            let quotient = contract(
                &workers, &mut Uninterrupted, &relations, &p, contraction, &BuildMemory::unlimited(),
            ).unwrap();
            let quotient_edges: Edges = quotient
                .edges()
                .zip(quotient.weights().unwrap())
                .map(|((r, s, t), &w)| (r.index(), s, t, w))
                .collect();
            let level = reference::graph(4, 3, &quotient_edges, true, false);
            let expected = undirected(&level, WEIGHTED, !undirected_graph);
            let view = undirected(&graph, Weighting { weighted, per_relation }, !undirected_graph);
            assert_same(&contracted(&view, &p), &expected);
        }

        /// Contracting a quotient by a partition of its nodes is
        /// contracting the original by the composed partition.
        #[test]
        fn contractions_compose(
            (n, edges, communities) in partitioned(),
            coarser in prop::collection::vec(0u64..3, 4),
            directed in any::<bool>(),
        ) {
            let graph = reference::graph(n, 3, &edges, true, false);
            let b = undirected(&graph, WEIGHTED, directed);
            let p = membership(&communities, 4);
            let q = membership(&coarser.iter().map(|&c| Some(c)).collect::<Vec<_>>(), 3);
            let composed: Vec<_> =
                communities.iter().map(|c| c.map(|c| coarser[c as usize])).collect();
            let twice = contracted(&contracted(&b, &p), &q);
            assert_same(&twice, &contracted(&b, &membership(&composed, 3)));
        }
    }
}
