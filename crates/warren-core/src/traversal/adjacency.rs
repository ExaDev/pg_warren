//! Which edges a traversal follows: the CSRs of a relation selection, read
//! in a direction (DESIGN.md §9.2, §9.3).

use super::TraversalError;
use std::iter::{FusedIterator, Peekable};

use crate::graph::{
    memory, AllRelations, AnyCsr, CsrRows, DenseId, Direction, GraphCsr, GraphError, RelationCsr,
    RelationIndex, RelationSelection, RowNeighbours, RowSweep,
};

/// The CSRs a traversal reads: the selected relations in one direction.
///
/// Unweighted traversals read the all-relations rows when every relation
/// is selected (the stored merged CSR, or the relations' rows merged on
/// the fly, which read the same). Weighted traversals read each relation's
/// CSR instead, because the merged CSR sums the weights of an edge present
/// in several relations, whereas a shortest path takes the cheapest of
/// them.
#[derive(Debug, Clone)]
pub struct Adjacency<'g> {
    direction: Direction,
    id_space: u64,
    relations: Selected<'g>,
    weighted_relations: Vec<&'g RelationCsr>,
    rows: Vec<CsrRows<'g>>,
    weighted_rows: Vec<&'g AnyCsr>,
    labelled: Vec<(RelationIndex, &'g RelationCsr)>,
    labelled_rows: Vec<LabelledRows<'g>>,
}

/// The relations an unweighted traversal scans: all of them, through the
/// all-relations rows, or the CSRs of each selected relation.
#[derive(Debug, Clone)]
enum Selected<'g> {
    All(AllRelations<'g>),
    Only(Vec<&'g RelationCsr>),
}

/// An [`Adjacency`]'s rows read with nodes mostly ascending: a
/// [`RowSweep`] per CSR set read.
#[derive(Debug)]
pub(crate) struct AdjacencySweep<'g>(Vec<RowSweep<'g>>);

impl<'g> AdjacencySweep<'g> {
    /// [`Adjacency::row_entries`].
    pub(crate) fn row_entries(
        &mut self,
        node: DenseId,
    ) -> impl Iterator<Item = DenseId> + use<'_, 'g> {
        self.0.iter_mut().flat_map(move |rows| rows.neighbours(node))
    }

    /// [`Adjacency::row_entry_bound`].
    pub(crate) fn row_entry_bound(&mut self, node: DenseId) -> u64 {
        self.0.iter_mut().map(|rows| rows.degree_bound(node)).sum()
    }
}

/// The CSRs one relation is read from: one, or two for [`Direction::Both`].
#[derive(Debug, Clone)]
struct LabelledRows<'g> {
    relation: RelationIndex,
    first: &'g AnyCsr,
    second: Option<&'g AnyCsr>,
}

impl<'g> Adjacency<'g> {
    /// The CSRs of `relations` in `graph`, read in `direction`.
    pub fn new(
        graph: &'g GraphCsr,
        direction: Direction,
        relations: RelationSelection<'_>,
    ) -> Result<Self, TraversalError> {
        let (relations, weighted_relations, labelled) = match relations {
            RelationSelection::All => {
                let each = graph.relations().iter().collect();
                let labelled = graph
                    .relations()
                    .iter()
                    .enumerate()
                    .map(|(position, csr)| Ok((RelationIndex::at(position)?, csr)))
                    .collect::<Result<Vec<_>, GraphError>>()?;
                (Selected::All(graph.all_relations()), each, labelled)
            }
            RelationSelection::Only(indexes) => {
                let mut indexes = indexes.to_vec();
                indexes.sort_unstable();
                indexes.dedup();
                let labelled = indexes
                    .into_iter()
                    .map(|relation| {
                        graph
                            .relation(relation)
                            .map(|csr| (relation, csr))
                            .ok_or(TraversalError::UnknownRelation { relation: relation.index() })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let selected: Vec<_> = labelled.iter().map(|&(_, csr)| csr).collect();
                (Selected::Only(selected.clone()), selected, labelled)
            }
        };
        let id_space = graph.all_relations().forward().id_space();
        Self::read(direction, id_space, relations, weighted_relations, labelled)
    }

    fn read(
        direction: Direction,
        id_space: u64,
        relations: Selected<'g>,
        weighted_relations: Vec<&'g RelationCsr>,
        labelled: Vec<(RelationIndex, &'g RelationCsr)>,
    ) -> Result<Self, TraversalError> {
        let rows = relations.rows(direction)?;
        let weighted_rows = scanned(&weighted_relations, direction)?;
        let labelled_rows = labelled
            .iter()
            .map(|&(relation, csr)| LabelledRows::read(relation, csr, direction))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            direction,
            id_space,
            relations,
            weighted_relations,
            rows,
            weighted_rows,
            labelled,
            labelled_rows,
        })
    }

    /// The same relations read in the opposite direction.
    pub fn reversed(&self) -> Result<Self, TraversalError> {
        Self::read(
            self.direction.reversed(),
            self.id_space,
            self.relations.clone(),
            self.weighted_relations.clone(),
            self.labelled.clone(),
        )
    }

    /// The direction the CSRs are read in.
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// The id space of the graph.
    pub fn id_space(&self) -> u64 {
        self.id_space
    }

    /// The distinct nodes one edge away from `node` across every selected
    /// relation (and, for [`Direction::Both`], both directions), in
    /// ascending dense-id order: a k-way merge of the sorted rows. Empty
    /// outside the id space.
    pub fn neighbours(&self, node: DenseId) -> DistinctNeighbours<'g> {
        DistinctNeighbours {
            rows: self.rows.iter().map(|csr| csr.neighbours(node).peekable()).collect(),
        }
    }

    /// [`Self::neighbours`] of `node`, collected into a buffer that borrows
    /// nothing, reserved up front for every row entry of `node` (a failed
    /// allocation is an error, not an abort).
    pub fn collect_neighbours(&self, node: DenseId) -> Result<Vec<DenseId>, TraversalError> {
        let entries = usize::try_from(self.row_entry_bound(node)).unwrap_or(usize::MAX);
        let mut collected = memory::with_capacity(entries)?;
        collected.extend(self.neighbours(node));
        Ok(collected)
    }

    /// The number of [`Self::neighbours`] of `node`.
    pub fn degree(&self, node: DenseId) -> u64 {
        self.neighbours(node).count() as u64
    }

    /// The row entries of `node` in every CSR read: a node reached through
    /// several CSRs (relations, or both directions) appears once per CSR.
    /// Traversals use this; their visited set absorbs the repeats.
    pub(crate) fn row_entries(&self, node: DenseId) -> impl Iterator<Item = DenseId> + '_ {
        self.rows.iter().flat_map(move |rows| rows.neighbours(node))
    }

    /// An upper bound on the number of [`Self::row_entries`] of `node`,
    /// in O(1) per CSR read; exact unless relations are merged on the fly.
    pub(crate) fn row_entry_bound(&self, node: DenseId) -> u64 {
        self.rows.iter().map(|rows| rows.degree_bound(node)).sum()
    }

    /// The rows read, for reading nodes mostly in ascending order
    /// ([`RowSweep`]), as a sorted frontier or a scan of the id space reads
    /// them.
    pub(crate) fn sweep(&self) -> Result<AdjacencySweep<'g>, TraversalError> {
        let mut sweeps = memory::with_capacity(self.rows.len())?;
        for rows in &self.rows {
            sweeps.push(rows.sweep()?);
        }
        Ok(AdjacencySweep(sweeps))
    }

    /// The hops from `node`, relation by relation in the selection's index
    /// order: each `(relation, neighbour)` pair once, so for
    /// [`Direction::Both`] a neighbour linked both ways by one relation (or
    /// by a self-loop) is one hop. Walks that report or count paths use
    /// this; walks that only need the nodes reached use
    /// [`Self::row_entries`].
    pub(crate) fn labelled_neighbours(
        &self,
        node: DenseId,
    ) -> impl Iterator<Item = (RelationIndex, DenseId)> + '_ {
        self.labelled_rows.iter().flat_map(move |rows| {
            let first = rows.first.neighbours(node).peekable();
            let second = rows.second.map(|csr| csr.neighbours(node).peekable());
            UnitedRow { first, second }.map(move |neighbour| (rows.relation, neighbour))
        })
    }

    /// The nodes one edge away from `node` with each edge's weight (1 when
    /// the graph is unweighted), one entry per relation the edge is in.
    pub(crate) fn weighted_neighbours(
        &self,
        node: DenseId,
    ) -> impl Iterator<Item = (DenseId, f64)> + '_ {
        self.weighted_rows.iter().flat_map(move |csr| {
            let weights = csr.neighbour_weights(node);
            csr.neighbours(node).enumerate().map(move |(i, target)| {
                (target, weights.and_then(|w| w.get(i)).map_or(1.0, |&w| w))
            })
        })
    }
}

/// The distinct neighbours of a node, ascending; see
/// [`Adjacency::neighbours`].
#[derive(Debug, Clone)]
pub struct DistinctNeighbours<'g> {
    rows: Vec<Peekable<RowNeighbours<'g>>>,
}

impl Iterator for DistinctNeighbours<'_> {
    type Item = DenseId;

    fn next(&mut self) -> Option<DenseId> {
        let smallest = self.rows.iter_mut().filter_map(|row| row.peek().copied()).min()?;
        for row in &mut self.rows {
            row.next_if_eq(&smallest);
        }
        Some(smallest)
    }
}

impl FusedIterator for DistinctNeighbours<'_> {}

impl<'g> LabelledRows<'g> {
    fn read(
        relation: RelationIndex,
        csr: &'g RelationCsr,
        direction: Direction,
    ) -> Result<Self, TraversalError> {
        let reverse = || csr.reverse().ok_or(TraversalError::ReverseCsrMissing);
        let (first, second) = match direction {
            Direction::Out => (csr.forward(), None),
            Direction::In => (reverse()?, None),
            Direction::Both => (csr.forward(), Some(reverse()?)),
        };
        Ok(Self { relation, first, second })
    }
}

/// One sorted row, or the ascending, distinct union of two.
struct UnitedRow<'g> {
    first: Peekable<crate::graph::Neighbours<'g>>,
    second: Option<Peekable<crate::graph::Neighbours<'g>>>,
}

impl Iterator for UnitedRow<'_> {
    type Item = DenseId;

    fn next(&mut self) -> Option<DenseId> {
        let Some(second) = &mut self.second else {
            return self.first.next();
        };
        let smallest = match (self.first.peek(), second.peek()) {
            (Some(&a), Some(&b)) => a.min(b),
            (Some(&a), None) => a,
            (None, Some(&b)) => b,
            (None, None) => return None,
        };
        self.first.next_if_eq(&smallest);
        second.next_if_eq(&smallest);
        Some(smallest)
    }
}

impl<'g> Selected<'g> {
    /// The rows to scan in `direction`.
    fn rows(&self, direction: Direction) -> Result<Vec<CsrRows<'g>>, TraversalError> {
        let all = match self {
            Self::All(all) => all,
            Self::Only(relations) => {
                let csrs = scanned(relations, direction)?;
                return Ok(csrs.into_iter().map(CsrRows::from).collect());
            }
        };
        let reverse = || all.reverse().ok_or(TraversalError::ReverseCsrMissing);
        Ok(match direction {
            Direction::Out => vec![all.forward()],
            Direction::In => vec![reverse()?],
            Direction::Both => vec![all.forward(), reverse()?],
        })
    }
}

/// The CSRs to scan for `relations` in `direction`.
fn scanned<'g>(
    relations: &[&'g RelationCsr],
    direction: Direction,
) -> Result<Vec<&'g AnyCsr>, TraversalError> {
    let mut rows = Vec::with_capacity(relations.len() * 2);
    for relation in relations {
        let reverse = || relation.reverse().ok_or(TraversalError::ReverseCsrMissing);
        match direction {
            Direction::Out => rows.push(relation.forward()),
            Direction::In => rows.push(reverse()?),
            Direction::Both => rows.extend([relation.forward(), reverse()?]),
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traversal::example::{directed, graph, relations};

    fn neighbours(adjacency: &Adjacency<'_>, node: DenseId) -> Vec<DenseId> {
        let mut found: Vec<DenseId> = adjacency.row_entries(node).collect();
        found.sort_unstable();
        found
    }

    #[test]
    fn out_follows_the_forward_csr_and_in_the_reverse() {
        let graph = directed(3, &[(0, 1), (1, 2), (2, 1)]);
        let out = Adjacency::new(&graph, Direction::Out, RelationSelection::All).unwrap();
        let into = Adjacency::new(&graph, Direction::In, RelationSelection::All).unwrap();
        assert_eq!(neighbours(&out, 1), [2]);
        assert_eq!(neighbours(&into, 1), [0, 2]);
        assert_eq!(out.id_space(), 3);
    }

    #[test]
    fn both_unites_the_forward_and_reverse_rows() {
        let graph = directed(3, &[(0, 1), (1, 2)]);
        let both = Adjacency::new(&graph, Direction::Both, RelationSelection::All).unwrap();
        assert_eq!(neighbours(&both, 1), [0, 2]);
    }

    #[test]
    fn in_and_both_need_a_reverse_csr() {
        let graph = graph(2, 1, &[(0, 0, 1, 1.0)], false);
        for direction in [Direction::In, Direction::Both] {
            assert_eq!(
                Adjacency::new(&graph, direction, RelationSelection::All).unwrap_err(),
                TraversalError::ReverseCsrMissing
            );
        }
        assert!(Adjacency::new(&graph, Direction::Out, RelationSelection::All).is_ok());
    }

    #[test]
    fn a_relation_selection_restricts_the_edges_followed() {
        let graph = graph(3, 3, &[(0, 0, 1, 1.0), (1, 0, 2, 1.0), (2, 1, 2, 1.0)], true);
        let r = relations(3);
        let only = [r[1], r[2], r[1]];
        let some = Adjacency::new(&graph, Direction::Out, RelationSelection::Only(&only)).unwrap();
        assert_eq!(neighbours(&some, 0), [2]);
        assert_eq!(neighbours(&some, 1), [2]);
        let none = Adjacency::new(&graph, Direction::Out, RelationSelection::Only(&[])).unwrap();
        assert_eq!(neighbours(&none, 0), [] as [DenseId; 0]);
    }

    #[test]
    fn public_neighbours_are_distinct_and_ascending_across_csrs() {
        // 0 → 1, 1 → 0 (in relation 0 and 1), 1 → 1, 1 → 2, 3 → 1
        let graph = graph(
            4,
            2,
            &[
                (0, 0, 1, 1.0),
                (0, 1, 0, 1.0),
                (1, 1, 0, 1.0),
                (0, 1, 1, 1.0),
                (1, 1, 2, 1.0),
                (0, 3, 1, 1.0),
            ],
            true,
        );
        let r = relations(2);
        let both = Adjacency::new(&graph, Direction::Both, RelationSelection::Only(&r)).unwrap();
        assert_eq!(both.neighbours(1).collect::<Vec<_>>(), [0, 1, 2, 3]);
        assert_eq!(both.degree(1), 4);
        let out = Adjacency::new(&graph, Direction::Out, RelationSelection::Only(&r)).unwrap();
        assert_eq!(out.neighbours(1).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(out.degree(1), 3);
        assert_eq!(out.neighbours(4).count(), 0);
        assert_eq!(out.degree(u64::MAX), 0);
    }

    #[test]
    fn collected_neighbours_are_the_distinct_neighbours_in_a_buffer_of_their_own() {
        // 0 → 1 and 0 → 2 in relations 0 and 1, 2 → 0 in relation 0
        let graph = graph(
            3,
            2,
            &[(0, 0, 1, 1.0), (1, 0, 1, 1.0), (0, 0, 2, 1.0), (1, 0, 2, 1.0), (0, 2, 0, 1.0)],
            true,
        );
        let both = Adjacency::new(&graph, Direction::Both, RelationSelection::All).unwrap();
        let collected = both.collect_neighbours(0).unwrap();
        drop(both);
        assert_eq!(collected, [1, 2]);
        let out = Adjacency::new(&graph, Direction::Out, RelationSelection::All).unwrap();
        assert_eq!(out.collect_neighbours(1).unwrap(), [] as [DenseId; 0]);
        assert_eq!(out.collect_neighbours(u64::MAX).unwrap(), [] as [DenseId; 0]);
    }

    #[test]
    fn labelled_neighbours_name_each_hop_s_relation_once() {
        // 0 → 1 in relations 0 and 1, 1 → 0 in relation 0, 0 → 0 in relation 1
        let graph =
            graph(2, 2, &[(0, 0, 1, 1.0), (1, 0, 1, 1.0), (0, 1, 0, 1.0), (1, 0, 0, 1.0)], true);
        let r = relations(2);
        let labelled = |direction, selection| {
            let adjacency = Adjacency::new(&graph, direction, selection).unwrap();
            adjacency.labelled_neighbours(0).collect::<Vec<_>>()
        };
        assert_eq!(
            labelled(Direction::Out, RelationSelection::All),
            [(r[0], 1), (r[1], 0), (r[1], 1)]
        );
        assert_eq!(labelled(Direction::In, RelationSelection::All), [(r[0], 1), (r[1], 0)]);
        assert_eq!(
            labelled(Direction::Both, RelationSelection::All),
            [(r[0], 1), (r[1], 0), (r[1], 1)],
            "both directions of one relation count each neighbour once"
        );
        assert_eq!(
            labelled(Direction::Both, RelationSelection::Only(&[r[1]])),
            [(r[1], 0), (r[1], 1)]
        );
        let reversed = Adjacency::new(&graph, Direction::Out, RelationSelection::All)
            .unwrap()
            .reversed()
            .unwrap();
        assert_eq!(reversed.labelled_neighbours(1).collect::<Vec<_>>(), [(r[0], 0), (r[1], 0)]);
    }

    #[test]
    fn an_unknown_relation_is_an_error() {
        let graph = graph(2, 1, &[(0, 0, 1, 1.0)], true);
        let unknown = relations(2)[1];
        assert_eq!(
            Adjacency::new(&graph, Direction::Out, RelationSelection::Only(&[unknown]))
                .unwrap_err(),
            TraversalError::UnknownRelation { relation: 1 }
        );
    }

    #[test]
    fn reversing_flips_the_direction() {
        let graph = directed(2, &[(0, 1)]);
        let out = Adjacency::new(&graph, Direction::Out, RelationSelection::All).unwrap();
        let reversed = out.reversed().unwrap();
        assert_eq!(reversed.direction(), Direction::In);
        assert_eq!(neighbours(&reversed, 1), [0]);
        assert_eq!(Direction::Both.reversed(), Direction::Both);
        assert_eq!(Direction::In.reversed(), Direction::Out);
    }

    #[test]
    fn weighted_neighbours_keep_each_relation_s_own_weight() {
        // The merged CSR sums 0 → 1 to 5; a weighted traversal sees 2 and 3.
        let graph = graph(2, 2, &[(0, 0, 1, 2.0), (1, 0, 1, 3.0)], true);
        let out = Adjacency::new(&graph, Direction::Out, RelationSelection::All).unwrap();
        let mut weighted: Vec<(DenseId, f64)> = out.weighted_neighbours(0).collect();
        weighted.sort_by(|a, b| a.1.total_cmp(&b.1));
        assert_eq!(weighted, [(1, 2.0), (1, 3.0)]);
        assert_eq!(neighbours(&out, 0), [1]);
    }

    #[test]
    fn an_unweighted_graph_has_unit_weights() {
        let graph = directed(2, &[(0, 1)]);
        let into = Adjacency::new(&graph, Direction::In, RelationSelection::All).unwrap();
        assert_eq!(into.weighted_neighbours(1).collect::<Vec<_>>(), [(0, 1.0)]);
    }
}

#[cfg(test)]
mod properties {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;
    use crate::traversal::reference::{directions, scenarios, selections, Scenario};

    proptest! {
        #[test]
        fn neighbours_match_the_distinct_naive_steps(
            (scenario, selection) in scenarios().prop_flat_map(|scenario| {
                let relation_count = scenario.relation_count;
                (Just(scenario), selections(relation_count))
            }),
            direction in directions(),
        ) {
            let graph = scenario.graph();
            let indexes = scenario.indexes(&selection);
            let adjacency = Adjacency::new(&graph, direction, Scenario::selection(&indexes)).unwrap();
            let steps = scenario.steps(direction, &selection);
            for node in 0..scenario.id_space {
                let expected: Vec<DenseId> = steps
                    .iter()
                    .filter(|&&(from, _, _)| from == node)
                    .map(|&(_, to, _)| to)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                prop_assert_eq!(adjacency.neighbours(node).collect::<Vec<_>>(), expected.clone());
                prop_assert_eq!(adjacency.degree(node), expected.len() as u64);
                let entries = adjacency.row_entries(node).count() as u64;
                prop_assert!(adjacency.row_entry_bound(node) >= entries);
            }
        }

        #[test]
        fn labelled_neighbours_match_the_naive_hops(
            (scenario, selection) in scenarios().prop_flat_map(|scenario| {
                let relation_count = scenario.relation_count;
                (Just(scenario), selections(relation_count))
            }),
            direction in directions(),
            wide in any::<bool>(),
        ) {
            let graph = if wide { scenario.wide_graph() } else { scenario.graph() };
            let indexes = scenario.indexes(&selection);
            let adjacency = Adjacency::new(&graph, direction, Scenario::selection(&indexes)).unwrap();
            let hops = scenario.hops(direction, &selection);
            for node in 0..scenario.id_space {
                let labelled: Vec<(usize, DenseId)> = adjacency
                    .labelled_neighbours(node)
                    .map(|(relation, neighbour)| (relation.index(), neighbour))
                    .collect();
                let distinct: BTreeSet<_> = labelled.iter().copied().collect();
                prop_assert_eq!(distinct.len(), labelled.len(), "a hop was repeated");
                let expected: BTreeSet<_> = hops
                    .iter()
                    .filter(|&&(from, _, _)| from == node)
                    .map(|&(_, relation, to)| (relation, to))
                    .collect();
                prop_assert_eq!(distinct, expected);
            }
        }

        /// Every traversal reads its graph through an `Adjacency`, so equal
        /// rows, in equal order, give equal results and work counts. The
        /// edges are spread over up to 12 relations, past the 8 rows a merge
        /// holds inline; targets are `u32` or `u64`, with reverse CSRs or
        /// without (then `in` and `both` fail alike).
        #[test]
        fn rows_read_the_same_with_or_without_the_merged_csr(
            (scenario, selection) in scenarios()
                .prop_flat_map(|scenario| (Just(scenario), 1usize..13))
                .prop_map(|(scenario, relation_count)| {
                    let edges = scenario.edges.iter().enumerate()
                        .map(|(i, &(r, s, t, w))| ((r + 5 * i) % relation_count, s, t, w))
                        .collect();
                    Scenario { relation_count, edges, ..scenario }
                })
                .prop_flat_map(|scenario| {
                    let relation_count = scenario.relation_count;
                    (Just(scenario), selections(relation_count))
                }),
            direction in directions(),
            build_reverse in any::<bool>(),
            wide in any::<bool>(),
        ) {
            let stored = scenario.graph_as(true, build_reverse, wide);
            let unmerged = scenario.graph_as(false, build_reverse, wide);
            let indexes = scenario.indexes(&selection);
            let read = |graph| Adjacency::new(graph, direction, Scenario::selection(&indexes));
            let (stored, unmerged) = match (read(&stored), read(&unmerged)) {
                (Ok(stored), Ok(unmerged)) => (stored, unmerged),
                (stored, unmerged) => {
                    prop_assert_eq!(unmerged.err(), stored.err());
                    return Ok(());
                }
            };
            let reversed = match (stored.reversed(), unmerged.reversed()) {
                (Ok(stored), Ok(unmerged)) => Some((stored, unmerged)),
                (stored, unmerged) => {
                    prop_assert_eq!(unmerged.err(), stored.err());
                    None
                }
            };
            for (stored, unmerged) in [(stored, unmerged)].into_iter().chain(reversed) {
                prop_assert_eq!(unmerged.id_space(), stored.id_space());
                for node in 0..=scenario.id_space {
                    let rows = |adjacency: &Adjacency<'_>| {
                        (
                            adjacency.row_entries(node).collect::<Vec<_>>(),
                            adjacency.neighbours(node).collect::<Vec<_>>(),
                            adjacency.labelled_neighbours(node).collect::<Vec<_>>(),
                            adjacency.weighted_neighbours(node).collect::<Vec<_>>(),
                        )
                    };
                    prop_assert_eq!(rows(&unmerged), rows(&stored));
                }
            }
        }
    }
}
