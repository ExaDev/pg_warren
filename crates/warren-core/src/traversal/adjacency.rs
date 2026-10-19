//! Which edges a traversal follows: the CSRs of a relation selection, read
//! in a direction (DESIGN.md §9.2, §9.3).

use super::TraversalError;
use crate::graph::{AnyCsr, DenseId, GraphCsr, RelationCsr, RelationIndex};

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

/// Which relations a traversal follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationSelection<'a> {
    /// Every relation.
    All,
    /// Only these relations; repeats are ignored.
    Only(&'a [RelationIndex]),
}

/// The CSRs a traversal reads: the selected relations in one direction.
///
/// Selecting every relation reads the merged CSR.
#[derive(Debug, Clone)]
pub struct Adjacency<'g> {
    direction: Direction,
    id_space: u64,
    relations: Vec<&'g RelationCsr>,
    rows: Vec<&'g AnyCsr>,
}

impl<'g> Adjacency<'g> {
    /// The CSRs of `relations` in `graph`, read in `direction`.
    pub fn new(
        graph: &'g GraphCsr,
        direction: Direction,
        relations: RelationSelection<'_>,
    ) -> Result<Self, TraversalError> {
        let relations = match relations {
            RelationSelection::All => vec![graph.all_relations()],
            RelationSelection::Only(indexes) => {
                let mut indexes = indexes.to_vec();
                indexes.sort_unstable();
                indexes.dedup();
                indexes
                    .into_iter()
                    .map(|relation| {
                        graph
                            .relation(relation)
                            .ok_or(TraversalError::UnknownRelation { relation: relation.index() })
                    })
                    .collect::<Result<Vec<_>, _>>()?
            }
        };
        let id_space = graph.all_relations().forward().id_space();
        Self::read(direction, id_space, relations)
    }

    fn read(
        direction: Direction,
        id_space: u64,
        relations: Vec<&'g RelationCsr>,
    ) -> Result<Self, TraversalError> {
        let rows = scanned(&relations, direction)?;
        Ok(Self { direction, id_space, relations, rows })
    }

    /// The same relations read in the opposite direction.
    pub fn reversed(&self) -> Result<Self, TraversalError> {
        Self::read(self.direction.reversed(), self.id_space, self.relations.clone())
    }

    /// The direction the CSRs are read in.
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// The id space of the graph.
    pub fn id_space(&self) -> u64 {
        self.id_space
    }

    /// The nodes one edge away from `node`. A node reached through several
    /// CSRs (relations, or both directions) appears once per CSR.
    pub(crate) fn neighbours(&self, node: DenseId) -> impl Iterator<Item = DenseId> + '_ {
        self.rows.iter().flat_map(move |csr| csr.neighbours(node))
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
        let mut found: Vec<DenseId> = adjacency.neighbours(node).collect();
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
}
