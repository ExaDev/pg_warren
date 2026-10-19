//! Small hand-written graphs for the traversal tests.

use crate::graph::{DenseId, EdgeList, GraphCsr, RelationDictionary, RelationIndex};

/// The relation indexes `0..count`.
pub(crate) fn relations(count: usize) -> Vec<RelationIndex> {
    let mut dictionary = RelationDictionary::new();
    (0..count).map(|r| dictionary.intern(&r.to_string()).unwrap()).collect()
}

/// A graph over `id_space` ids from `(relation, source, target, weight)`
/// edges, weighted if any weight differs from 1, with a reverse CSR if
/// `reverse`.
pub(crate) fn graph(
    id_space: u64,
    relation_count: usize,
    edges: &[(usize, DenseId, DenseId, f64)],
    reverse: bool,
) -> GraphCsr {
    let indexes = relations(relation_count);
    let weighted = edges.iter().any(|&(_, _, _, weight)| weight != 1.0);
    let mut list = EdgeList::new(weighted);
    for &(relation, source, target, weight) in edges {
        list.push(indexes[relation], source, target, weight).unwrap();
    }
    GraphCsr::build(list, relation_count, id_space, reverse).unwrap()
}

/// A directed, unweighted, single-relation graph with a reverse CSR.
pub(crate) fn directed(id_space: u64, edges: &[(DenseId, DenseId)]) -> GraphCsr {
    let edges: Vec<_> = edges.iter().map(|&(source, target)| (0, source, target, 1.0)).collect();
    graph(id_space, 1, &edges, true)
}
