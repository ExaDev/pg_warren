//! Contraction (DESIGN.md §6.6, §6.11): the edges of the quotient graph,
//! one node per community, each edge between communities the aggregate of
//! the parent's edges between their members: `PᵀAP`.

use crate::compute::{ComputeError, Workers};
use crate::graph::{
    AnyCsr, BuildBuffer, BuildMemory, DenseId, EdgeList, GraphError, Membership, RelationIndex,
};
use crate::interrupts::{Cancellation, Interrupts, Work};

use super::CommunityError;

/// How a contraction aggregates edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Contraction {
    /// Sum the edges' weights (true) or count the edges (false).
    pub weighted: bool,
    /// Counting, an edge in several of the parent's relations counts once
    /// in each; else once, in the first of them (DESIGN.md §6.6).
    pub per_relation: bool,
    /// Leave out the edges inside a community.
    pub drop_self_loops: bool,
    /// The parent is undirected: each of its edges is stored both ways
    /// (a self-loop once) and counts once.
    pub undirected: bool,
}

/// The quotient's edges, relation by relation: `relations` pairs each of
/// the parent's relations' forward CSR, in relation order, with the
/// quotient relation its edges go to. Between two communities `c` and `d`
/// there is an edge `c → d` if some member of `c` has an edge to some
/// member of `d`, weighted by the sum of those edges' weights (or their
/// count, if not `weighted` or the parent has no weights): `PᵀAP` of the
/// stored matrix. Counting, an edge in several relations counts once, in
/// the first of them, so the quotient's relations sum to `PᵀAP` of the
/// all-relations `A` that `spmv` reads (1 for such an edge); or once in
/// each if `per_relation`. An undirected parent is
/// stored both ways (a self-loop once), so its quotient is undirected too:
/// an edge between two communities adds its weight to both directions, an
/// edge inside a community adds it twice to the community's self-loop, and
/// a self-loop once, as each counts in its nodes' degrees, so every
/// community's weighted degree is the sum of its members'.
///
/// Gustavson's row-by-row product: the members of each community in turn,
/// in ascending dense id, accumulate their rows' entries into a dense row
/// of the quotient, so each sum is taken in one order whatever the thread
/// count. It runs as one job on the compute pool, checking its
/// cancellation flag every 65,536 row entries; its scratch (the members
/// grouped by community and the dense row) and the edge list it returns
/// are charged to `memory`.
pub fn contract<I: Interrupts>(
    workers: &Workers,
    interrupts: &mut I,
    relations: &[(RelationIndex, &AnyCsr)],
    membership: &Membership,
    contraction: Contraction,
    memory: &BuildMemory,
) -> Result<EdgeList, CommunityError> {
    workers.run(interrupts, |cancellation| {
        Ok(aggregate(cancellation, relations, membership, contraction, memory))
    })?
}

/// [`contract`]'s work, on the pool.
fn aggregate(
    cancellation: &Cancellation,
    relations: &[(RelationIndex, &AnyCsr)],
    membership: &Membership,
    contraction: Contraction,
    memory: &BuildMemory,
) -> Result<EdgeList, CommunityError> {
    let mut work = Work::new(cancellation);
    let members = Members::group(membership, memory, &mut work)?;
    let mut row = QuotientRow::new(membership.quotient_id_space(), memory)?;
    let mut edges = EdgeList::within(membership.quotient_id_space(), true, memory);
    for (position, &(relation, csr)) in relations.iter().enumerate() {
        let earlier = &relations[..position];
        for community in 0..membership.quotient_id_space() {
            for &member in members.of(community) {
                work.spend(csr.degree(member) + 1).map_err(ComputeError::from)?;
                let weights = csr.neighbour_weights(member).filter(|_| contraction.weighted);
                // Counted once, an edge counts in the first relation it is in.
                let once = weights.is_none() && !contraction.per_relation;
                for (at, target) in csr.neighbours(member).enumerate() {
                    let Some(other) = membership.community(target) else { continue };
                    if once && earlier.iter().any(|(_, csr)| csr.has_edge(member, target)) {
                        continue;
                    }
                    // The upper triangle of an undirected quotient, mirrored
                    // below, so both directions hold one sum.
                    let counted = !contraction.undirected || other >= community;
                    if !counted || (contraction.drop_self_loops && other == community) {
                        continue;
                    }
                    let weight = weights.map_or(1.0, |weights| weights[at]);
                    row.add(other, weight)?;
                }
            }
            row.drain(|other, weight| {
                edges.push(relation, community, other, weight)?;
                if contraction.undirected && other != community {
                    edges.push(relation, other, community, weight)?;
                }
                Ok(())
            })?;
        }
    }
    edges.shrink_to_fit();
    Ok(edges)
}

/// The parent's nodes grouped by community, each group ascending.
struct Members {
    /// Where each community's members start in `nodes`; one more entry
    /// than there are communities.
    starts: BuildBuffer<u64>,
    nodes: BuildBuffer<DenseId>,
}

impl Members {
    /// Counting-sorts the nodes that belong to a community by community.
    fn group(
        membership: &Membership,
        memory: &BuildMemory,
        work: &mut Work<'_>,
    ) -> Result<Self, CommunityError> {
        let communities = usize::try_from(membership.quotient_id_space())
            .map_err(|_| GraphError::IdSpaceTooLarge)?;
        let mut starts = BuildBuffer::filled(memory, communities + 1, 0u64)?;
        let mut members = 0;
        for node in 0..membership.id_space() {
            work.spend(1).map_err(ComputeError::from)?;
            if let Some(community) = membership.community(node) {
                starts[community as usize + 1] += 1;
                members += 1;
            }
        }
        for community in 0..communities {
            starts[community + 1] += starts[community];
        }
        let mut nodes = BuildBuffer::filled(memory, members, 0)?;
        let mut next = BuildBuffer::with_capacity(memory, communities)?;
        next.extend(starts[..communities].iter().copied());
        for node in 0..membership.id_space() {
            work.spend(1).map_err(ComputeError::from)?;
            if let Some(community) = membership.community(node) {
                let at = &mut next[community as usize];
                nodes[*at as usize] = node;
                *at += 1;
            }
        }
        Ok(Self { starts, nodes })
    }

    /// The members of `community`, ascending.
    fn of(&self, community: DenseId) -> &[DenseId] {
        let start = self.starts[community as usize] as usize;
        let end = self.starts[community as usize + 1] as usize;
        &self.nodes[start..end]
    }
}

/// One row of the quotient as it is summed: a dense row of weights, which
/// of its entries the row has touched, and in what order.
struct QuotientRow {
    weights: BuildBuffer<f64>,
    touched: BuildBuffer<bool>,
    order: BuildBuffer<DenseId>,
}

impl QuotientRow {
    /// An empty row of `id_space` entries.
    fn new(id_space: u64, memory: &BuildMemory) -> Result<Self, CommunityError> {
        let len = usize::try_from(id_space).map_err(|_| GraphError::IdSpaceTooLarge)?;
        Ok(Self {
            weights: BuildBuffer::filled(memory, len, 0.0)?,
            touched: BuildBuffer::filled(memory, len, false)?,
            order: BuildBuffer::new(memory),
        })
    }

    /// Adds `weight` to entry `at`.
    fn add(&mut self, at: DenseId, weight: f64) -> Result<(), CommunityError> {
        let index = at as usize;
        if !self.touched[index] {
            self.touched[index] = true;
            self.weights[index] = 0.0;
            self.order.push(at)?;
        }
        self.weights[index] += weight;
        Ok(())
    }

    /// Passes each touched entry and its sum to `each`, in the order first
    /// touched, and leaves the row empty.
    fn drain(
        &mut self,
        mut each: impl FnMut(DenseId, f64) -> Result<(), CommunityError>,
    ) -> Result<(), CommunityError> {
        for &at in self.order.iter() {
            self.touched[at as usize] = false;
            each(at, self.weights[at as usize])?;
        }
        self.order.truncate(0);
        Ok(())
    }
}

// Not under Miri: the compute pool's work stealing runs through
// crossbeam-epoch, which Miri's Stacked Borrows model rejects.
#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::OnceLock;

    use proptest::prelude::*;

    use super::*;
    use crate::community::{modularity, Undirected, Weighting};
    use crate::compute::reference::{self, Edges};
    use crate::compute::{ComputeError, ComputeMemory};
    use crate::graph::{DenseId, GraphCsr, GraphError};
    use crate::interrupts::{Cancelled, Uninterrupted};

    fn workers() -> &'static Workers {
        static WORKERS: OnceLock<Workers> = OnceLock::new();
        WORKERS.get_or_init(|| Workers::new(2).unwrap())
    }

    /// Quotient edges: (relation, source, target) → weight.
    type Quotient = BTreeMap<(usize, DenseId, DenseId), f64>;

    /// The parent's CSRs: undirected edges are stored both ways, a
    /// self-loop once, as an undirected build stores them.
    fn parent(id_space: u64, relations: usize, edges: &Edges, undirected: bool) -> GraphCsr {
        let mut stored = edges.clone();
        if undirected {
            stored.extend(edges.iter().filter(|e| e.1 != e.2).map(|&(r, s, t, w)| (r, t, s, w)));
        }
        reference::graph(id_space, relations, &stored, true, false)
    }

    fn quotient(
        graph: &GraphCsr,
        membership: &Membership,
        contraction: Contraction,
    ) -> Result<Quotient, CommunityError> {
        let indexes = reference::relations(graph.relations().len());
        let relations: Vec<_> =
            indexes.iter().zip(graph.relations()).map(|(&r, csr)| (r, csr.forward())).collect();
        let memory = BuildMemory::unlimited();
        let edges =
            contract(workers(), &mut Uninterrupted, &relations, membership, contraction, &memory)?;
        let weights = edges.weights().unwrap();
        let mut found = Quotient::new();
        for ((relation, source, target), &weight) in edges.edges().zip(weights) {
            let previous = found.insert((relation.index(), source, target), weight);
            assert_eq!(previous, None, "the quotient's edges are aggregated");
        }
        Ok(found)
    }

    const WEIGHTED: Weighting = Weighting { weighted: true, per_relation: false };

    const DIRECTED: Contraction = Contraction {
        weighted: true,
        per_relation: false,
        drop_self_loops: false,
        undirected: false,
    };
    const UNDIRECTED: Contraction = Contraction { undirected: true, ..DIRECTED };

    fn membership(communities: &[Option<DenseId>], quotient_id_space: u64) -> Membership {
        Membership::new(communities, quotient_id_space).unwrap()
    }

    #[test]
    fn directed_edges_between_communities_are_summed() {
        // Communities {0, 1} → 0, {2, 3} → 1, {5} → 2; node 4 is in none.
        let edges: Edges = vec![
            (0, 0, 2, 1.0),
            (0, 1, 3, 2.0),
            (0, 3, 0, 4.0),
            (0, 0, 1, 8.0),
            (0, 1, 0, 16.0),
            (0, 5, 5, 32.0),
            (0, 2, 5, 64.0),
        ];
        let graph = parent(6, 1, &edges, false);
        let p = membership(&[Some(0), Some(0), Some(1), Some(1), None, Some(2)], 3);
        let expected = Quotient::from([
            ((0, 0, 0), 24.0),
            ((0, 0, 1), 3.0),
            ((0, 1, 0), 4.0),
            ((0, 1, 2), 64.0),
            ((0, 2, 2), 32.0),
        ]);
        assert_eq!(quotient(&graph, &p, DIRECTED).unwrap(), expected);
        let counted = Contraction { weighted: false, ..DIRECTED };
        let counts: Quotient =
            expected.keys().zip([2.0, 2.0, 1.0, 1.0, 1.0]).map(|(&k, w)| (k, w)).collect();
        assert_eq!(quotient(&graph, &p, counted).unwrap(), counts);
        let no_loops = Contraction { drop_self_loops: true, ..DIRECTED };
        let between: Quotient = expected.into_iter().filter(|((_, s, t), _)| s != t).collect();
        assert_eq!(quotient(&graph, &p, no_loops).unwrap(), between);
    }

    #[test]
    fn an_undirected_edge_inside_a_community_counts_both_ways() {
        // Undirected: 0 – 1 (inside 0), 1 – 2 and 0 – 2 (between), 2 – 2
        // (a self-loop, stored once), 2 – 3 (inside 1). The quotient is
        // PᵀAP of the stored matrix: an edge inside counts twice, a
        // self-loop once, as each counts in its nodes' degrees.
        let edges: Edges =
            vec![(0, 0, 1, 1.0), (0, 1, 2, 2.0), (0, 0, 2, 4.0), (0, 2, 2, 8.0), (0, 2, 3, 16.0)];
        let graph = parent(4, 1, &edges, true);
        let p = membership(&[Some(0), Some(0), Some(1), Some(1)], 2);
        let expected = Quotient::from([
            ((0, 0, 0), 2.0),
            ((0, 0, 1), 6.0),
            ((0, 1, 0), 6.0),
            ((0, 1, 1), 40.0),
        ]);
        assert_eq!(quotient(&graph, &p, UNDIRECTED).unwrap(), expected);
        let counted = Contraction { weighted: false, ..UNDIRECTED };
        let counts = Quotient::from([
            ((0, 0, 0), 2.0),
            ((0, 0, 1), 2.0),
            ((0, 1, 0), 2.0),
            ((0, 1, 1), 3.0),
        ]);
        assert_eq!(quotient(&graph, &p, counted).unwrap(), counts);
    }

    #[test]
    fn each_relation_is_contracted_on_its_own() {
        let edges: Edges = vec![(0, 0, 1, 1.0), (1, 0, 1, 2.0), (1, 1, 0, 4.0)];
        let graph = parent(2, 2, &edges, false);
        let p = membership(&[Some(1), Some(0)], 2);
        let expected = Quotient::from([((0, 1, 0), 1.0), ((1, 0, 1), 4.0), ((1, 1, 0), 2.0)]);
        assert_eq!(quotient(&graph, &p, DIRECTED).unwrap(), expected);
    }

    #[test]
    fn a_counted_edge_in_several_relations_counts_once_in_the_first() {
        // 0 → 1 in relations 0 and 1, 1 → 0 in relation 1 only, 2 → 3 in
        // relations 1 and 2; communities {0} → 0, {1} → 1, {2, 3} → 2.
        let edges: Edges =
            vec![(0, 0, 1, 1.0), (1, 0, 1, 2.0), (1, 1, 0, 4.0), (1, 2, 3, 8.0), (2, 2, 3, 16.0)];
        let graph = parent(4, 3, &edges, false);
        let p = membership(&[Some(0), Some(1), Some(2), Some(2)], 3);
        let counted = Contraction { weighted: false, ..DIRECTED };
        let once = Quotient::from([((0, 0, 1), 1.0), ((1, 1, 0), 1.0), ((1, 2, 2), 1.0)]);
        assert_eq!(quotient(&graph, &p, counted).unwrap(), once);
        let per_relation = Contraction { per_relation: true, ..counted };
        let each = Quotient::from([
            ((0, 0, 1), 1.0),
            ((1, 0, 1), 1.0),
            ((1, 1, 0), 1.0),
            ((1, 2, 2), 1.0),
            ((2, 2, 2), 1.0),
        ]);
        assert_eq!(quotient(&graph, &p, per_relation).unwrap(), each);
        // Weights are summed in each relation whether or not per relation.
        let summed = Quotient::from([
            ((0, 0, 1), 1.0),
            ((1, 0, 1), 2.0),
            ((1, 1, 0), 4.0),
            ((1, 2, 2), 8.0),
            ((2, 2, 2), 16.0),
        ]);
        assert_eq!(quotient(&graph, &p, DIRECTED).unwrap(), summed);
        let weighted = Contraction { per_relation: true, ..DIRECTED };
        assert_eq!(quotient(&graph, &p, weighted).unwrap(), summed);
    }

    /// The quotient by dense matrices, from the edge list: `PᵀA_rP` of
    /// each relation's `A_r` (an undirected parent's edges both ways, a
    /// self-loop once), counting an edge in an earlier relation only there
    /// unless weighted or per relation.
    fn naive(
        id_space: u64,
        edges: &Edges,
        relations: usize,
        p: &[Option<DenseId>],
        contraction: Contraction,
    ) -> Quotient {
        let mut stored = edges.clone();
        if contraction.undirected {
            stored.extend(edges.iter().filter(|e| e.1 != e.2).map(|&(r, s, t, w)| (r, t, s, w)));
        }
        let a: Vec<_> = (0..relations)
            .map(|r| reference::adjacency(id_space, &stored, Some(&[r]), true))
            .collect();
        let once = !contraction.weighted && !contraction.per_relation;
        let mut found = Quotient::new();
        for relation in 0..relations {
            for (u, row) in a[relation].iter().enumerate() {
                for (v, entry) in row.iter().enumerate() {
                    let (Some(w), Some(c), Some(d)) = (entry, p[u], p[v]) else { continue };
                    if contraction.drop_self_loops && c == d {
                        continue;
                    }
                    if once && a[..relation].iter().any(|earlier| earlier[u][v].is_some()) {
                        continue;
                    }
                    let value = if contraction.weighted { *w } else { 1.0 };
                    *found.entry((relation, c, d)).or_insert(0.0) += value;
                }
            }
        }
        found
    }

    fn close(a: &Quotient, b: &Quotient) -> bool {
        a.len() == b.len()
            && a.iter().zip(b).all(|((ka, wa), (kb, wb))| {
                ka == kb && (wa - wb).abs() <= 1e-9 * wa.abs().max(wb.abs()).max(1.0)
            })
    }

    proptest! {
        #[test]
        fn the_quotient_is_p_transpose_a_p(
            (id_space, edges, communities) in (1u64..24).prop_flat_map(|n| (
                Just(n),
                prop::collection::vec(
                    (0usize..2, 0..n, 0..n, (-8i32..8).prop_map(|w| f64::from(w) / 4.0)),
                    0..80,
                ),
                prop::collection::vec(prop::option::weighted(0.9, 0u64..6), n as usize),
            )),
            weighted in any::<bool>(),
            per_relation in any::<bool>(),
            drop_self_loops in any::<bool>(),
            undirected in any::<bool>(),
        ) {
            // A node in no community has no edges, as a tombstoned node.
            let edges: Edges = edges
                .into_iter()
                .filter(|&(_, s, t, _)| communities[s as usize].is_some() && communities[t as usize].is_some())
                .collect();
            let contraction = Contraction { weighted, per_relation, drop_self_loops, undirected };
            let graph = parent(id_space, 2, &edges, undirected);
            let p = membership(&communities, 6);
            let found = quotient(&graph, &p, contraction).unwrap();
            let expected = naive(id_space, &edges, 2, &communities, contraction);
            prop_assert!(close(&found, &expected), "{found:?} != {expected:?}");
        }
    }

    proptest! {
        #[test]
        fn a_communitys_weighted_degree_is_its_members(
            (id_space, edges, communities) in (1u64..20).prop_flat_map(|n| (
                Just(n),
                prop::collection::vec((0..n, 0..n, 1..5u32), 0..60),
                prop::collection::vec(0u64..4, n as usize),
            )),
            undirected in any::<bool>(),
        ) {
            let edges: Edges =
                edges.into_iter().map(|(s, t, w)| (0, s, t, f64::from(w))).collect();
            let graph = parent(id_space, 1, &edges, undirected);
            let p = membership(&communities.iter().map(|&c| Some(c)).collect::<Vec<_>>(), 4);
            let contraction = Contraction { undirected, ..DIRECTED };
            let found = quotient(&graph, &p, contraction).unwrap();
            let csr = graph.relations()[0].forward();
            for c in 0..4u64 {
                let members: f64 = (0..id_space)
                    .filter(|&u| communities[u as usize] == c)
                    .flat_map(|u| csr.neighbour_weights(u).unwrap().to_vec())
                    .sum();
                let row: f64 = found.iter().filter(|((_, s, _), _)| *s == c).map(|(_, w)| w).sum();
                prop_assert_eq!(row, members);
            }
        }
    }

    proptest! {
        #[test]
        fn a_partitions_modularity_is_its_quotients_singletons(
            (id_space, edges, communities) in (1u64..20).prop_flat_map(|n| (
                Just(n),
                prop::collection::vec((0usize..3, 0..n, 0..n, 1..5u32), 0..60),
                prop::collection::vec(0u64..4, n as usize),
            )),
            weighted in any::<bool>(),
            per_relation in any::<bool>(),
            undirected in any::<bool>(),
            resolution in prop::sample::select(vec![0.5, 1.0, 2.0]),
        ) {
            // In both counting modes, weighted or not, the quotient's
            // all-relations view is the parent's contracted.
            let edges: Edges =
                edges.into_iter().map(|(r, s, t, w)| (r, s, t, f64::from(w))).collect();
            let graph = parent(id_space, 3, &edges, undirected);
            let p = membership(&communities.iter().map(|&c| Some(c)).collect::<Vec<_>>(), 4);
            let contraction = Contraction { weighted, per_relation, undirected, ..DIRECTED };
            let found = quotient(&graph, &p, contraction).unwrap();
            let stored: Edges = found.iter().map(|(&(r, s, t), &w)| (r, s, t, w)).collect();
            let level = reference::graph(4, 3, &stored, true, false);
            let memory = ComputeMemory::new(u64::MAX);
            let view = |graph: &GraphCsr, weighting| {
                Undirected::new(&memory, &Cancellation::new(), graph, weighting, !undirected)
                    .unwrap()
            };
            let labels: Vec<i64> = communities.iter().map(|&c| c as i64).collect();
            let weighting = Weighting { weighted, per_relation };
            let before = modularity(&memory, &view(&graph, weighting), &labels, resolution).unwrap();
            let after =
                modularity(&memory, &view(&level, WEIGHTED), &[0, 1, 2, 3], resolution).unwrap();
            prop_assert!((before - after).abs() <= 1e-12, "{before} != {after}");
        }
    }

    #[test]
    fn the_sum_is_the_same_on_any_number_of_threads() {
        let edges: Edges = (0..400u64).map(|i| (0, i % 37, (i * 7) % 41, 0.1 * i as f64)).collect();
        let graph = parent(41, 1, &edges, false);
        let communities: Vec<_> = (0..41).map(|n| Some(n % 5)).collect();
        let p = membership(&communities, 5);
        let first = quotient(&graph, &p, DIRECTED).unwrap();
        let relations = [(reference::relations(1)[0], graph.relations()[0].forward())];
        let one = Workers::new(1).unwrap();
        let memory = BuildMemory::unlimited();
        let again = contract(&one, &mut Uninterrupted, &relations, &p, DIRECTED, &memory).unwrap();
        let bits = |q: &Quotient| q.values().map(|w| w.to_bits()).collect::<Vec<_>>();
        let again: Quotient = again
            .edges()
            .zip(again.weights().unwrap())
            .map(|((r, s, t), &w)| ((r.index(), s, t), w))
            .collect();
        assert_eq!(bits(&first), bits(&again));
    }

    #[test]
    fn its_scratch_and_edges_are_charged_to_the_build_memory() {
        let edges: Edges = (0..50u64).map(|i| (0, i, (i + 1) % 50, 1.0)).collect();
        let graph = parent(50, 1, &edges, false);
        let communities: Vec<_> = (0..50).map(Some).collect();
        let p = membership(&communities, 50);
        let relations = [(reference::relations(1)[0], graph.relations()[0].forward())];
        let memory = BuildMemory::new(512);
        let refused = contract(workers(), &mut Uninterrupted, &relations, &p, DIRECTED, &memory);
        assert!(
            matches!(refused, Err(CommunityError::Graph(GraphError::BuildMemoryLimit { .. }))),
            "{refused:?}"
        );
        assert_eq!(memory.in_use(), 0);
    }

    #[test]
    fn a_cancel_stops_it() {
        struct Cancel;
        impl Interrupts for Cancel {
            fn check(&mut self) -> Result<(), Cancelled> {
                Err(Cancelled)
            }
        }
        let graph = parent(2, 1, &vec![(0, 0, 1, 1.0)], false);
        let p = membership(&[Some(0), Some(0)], 1);
        let relations = [(reference::relations(1)[0], graph.relations()[0].forward())];
        let memory = BuildMemory::unlimited();
        assert_eq!(
            contract(workers(), &mut Cancel, &relations, &p, DIRECTED, &memory).map(|_| ()),
            Err(CommunityError::Compute(ComputeError::Cancelled))
        );
    }
}
