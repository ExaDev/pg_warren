//! Which edges a traversal follows: the CSRs of a relation selection, read
//! in a direction (DESIGN.md §9.2, §9.3).

use super::TraversalError;
use std::iter::{FusedIterator, Peekable};

use crate::graph::{AnyCsr, DenseId, GraphCsr, Neighbours, RelationCsr, RelationSelection};

/// Which way a traversal follows edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// From source to target: the forward CSR.
    Out,
    /// From target to source: the reverse CSR.
    In,
    /// Either way: the forward and reverse CSRs together.
    Both,
}

impl Direction {
    /// The opposite direction; `Both` is its own opposite.
    pub fn reversed(self) -> Self {
        match self {
            Self::Out => Self::In,
            Self::In => Self::Out,
            Self::Both => Self::Both,
        }
    }
}

/// The CSRs a traversal reads: the selected relations in one direction.
///
/// Unweighted traversals read the merged CSR when every relation is
/// selected. Weighted traversals read each relation's CSR instead, because
/// the merged CSR sums the weights of an edge present in several relations,
/// whereas a shortest path takes the cheapest of them.
#[derive(Debug, Clone)]
pub struct Adjacency<'g> {
    direction: Direction,
    id_space: u64,
    relations: Vec<&'g RelationCsr>,
    weighted_relations: Vec<&'g RelationCsr>,
    rows: Vec<&'g AnyCsr>,
    weighted_rows: Vec<&'g AnyCsr>,
}

impl<'g> Adjacency<'g> {
    /// The CSRs of `relations` in `graph`, read in `direction`.
    pub fn new(
        graph: &'g GraphCsr,
        direction: Direction,
        relations: RelationSelection<'_>,
    ) -> Result<Self, TraversalError> {
        let (relations, weighted_relations) = match relations {
            RelationSelection::All => {
                let each = match graph.relations() {
                    [] => vec![graph.all_relations()],
                    each => each.iter().collect(),
                };
                (vec![graph.all_relations()], each)
            }
            RelationSelection::Only(indexes) => {
                let mut indexes = indexes.to_vec();
                indexes.sort_unstable();
                indexes.dedup();
                let selected = indexes
                    .into_iter()
                    .map(|relation| {
                        graph
                            .relation(relation)
                            .ok_or(TraversalError::UnknownRelation { relation: relation.index() })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                (selected.clone(), selected)
            }
        };
        let id_space = graph.all_relations().forward().id_space();
        Self::read(direction, id_space, relations, weighted_relations)
    }

    fn read(
        direction: Direction,
        id_space: u64,
        relations: Vec<&'g RelationCsr>,
        weighted_relations: Vec<&'g RelationCsr>,
    ) -> Result<Self, TraversalError> {
        let rows = scanned(&relations, direction)?;
        let weighted_rows = scanned(&weighted_relations, direction)?;
        Ok(Self { direction, id_space, relations, weighted_relations, rows, weighted_rows })
    }

    /// The same relations read in the opposite direction.
    pub fn reversed(&self) -> Result<Self, TraversalError> {
        Self::read(
            self.direction.reversed(),
            self.id_space,
            self.relations.clone(),
            self.weighted_relations.clone(),
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

    /// The number of [`Self::neighbours`] of `node`.
    pub fn degree(&self, node: DenseId) -> u64 {
        self.neighbours(node).count() as u64
    }

    /// The row entries of `node` in every CSR read: a node reached through
    /// several CSRs (relations, or both directions) appears once per CSR.
    /// Traversals use this; their visited set absorbs the repeats.
    pub(crate) fn row_entries(&self, node: DenseId) -> impl Iterator<Item = DenseId> + '_ {
        self.rows.iter().flat_map(move |csr| csr.neighbours(node))
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
    rows: Vec<Peekable<Neighbours<'g>>>,
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
            }
        }
    }
}
