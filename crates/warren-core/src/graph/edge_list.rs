//! The edge list of a build: edges remapped from external ids to dense ids,
//! with edges whose endpoints are NULL or unknown dropped and counted
//! (DESIGN.md §5.4 step 3).

use super::{memory, DenseId, ExternalId, GraphError, IdMap, RelationIndex};

/// Edges in dense ids, as parallel arrays, in the order they were pushed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EdgeList {
    relations: Vec<RelationIndex>,
    sources: Vec<DenseId>,
    targets: Vec<DenseId>,
    weights: Option<Vec<f64>>,
}

impl EdgeList {
    /// An empty edge list, recording weights if `weighted`.
    pub fn new(weighted: bool) -> Self {
        Self { weights: weighted.then(Vec::new), ..Self::default() }
    }

    /// Appends an edge. `weight` is ignored by an unweighted list.
    pub fn push(
        &mut self,
        relation: RelationIndex,
        source: DenseId,
        target: DenseId,
        weight: f64,
    ) -> Result<(), GraphError> {
        memory::reserve_one(&mut self.relations)?;
        memory::reserve_one(&mut self.sources)?;
        memory::reserve_one(&mut self.targets)?;
        self.relations.push(relation);
        self.sources.push(source);
        self.targets.push(target);
        if let Some(weights) = &mut self.weights {
            memory::reserve_one(weights)?;
            weights.push(weight);
        }
        Ok(())
    }

    /// The number of edges.
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    /// Whether the list has no edges.
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// The relation of each edge.
    pub fn relations(&self) -> &[RelationIndex] {
        &self.relations
    }

    /// The source dense id of each edge.
    pub fn sources(&self) -> &[DenseId] {
        &self.sources
    }

    /// The target dense id of each edge.
    pub fn targets(&self) -> &[DenseId] {
        &self.targets
    }

    /// The weight of each edge, if the list is weighted.
    pub fn weights(&self) -> Option<&[f64]> {
        self.weights.as_deref()
    }
}

/// Remaps edges from external ids to dense ids through an id map.
#[derive(Debug)]
pub struct EdgeIntake<'m, E> {
    id_map: &'m IdMap<E>,
    directed: bool,
    edges: EdgeList,
    dropped_edges: u64,
}

impl<'m, E: ExternalId> EdgeIntake<'m, E> {
    /// An intake for a directed graph.
    pub fn directed(id_map: &'m IdMap<E>, weighted: bool) -> Self {
        Self::new(id_map, true, weighted)
    }

    /// An intake for an undirected graph: each edge is emitted in both
    /// directions with the same weight, except a self-loop, which is emitted
    /// once.
    pub fn undirected(id_map: &'m IdMap<E>, weighted: bool) -> Self {
        Self::new(id_map, false, weighted)
    }

    fn new(id_map: &'m IdMap<E>, directed: bool, weighted: bool) -> Self {
        Self { id_map, directed, edges: EdgeList::new(weighted), dropped_edges: 0 }
    }

    /// Takes one edge row. An edge with a NULL (`None`) endpoint, or one that
    /// is not a live node of the id map, is dropped and counted.
    pub fn push(
        &mut self,
        relation: RelationIndex,
        source: Option<&E>,
        target: Option<&E>,
        weight: f64,
    ) -> Result<(), GraphError> {
        let source = source.and_then(|id| self.id_map.dense_id(id));
        let target = target.and_then(|id| self.id_map.dense_id(id));
        let (Some(source), Some(target)) = (source, target) else {
            self.dropped_edges += 1;
            return Ok(());
        };
        self.edges.push(relation, source, target, weight)?;
        if !self.directed && source != target {
            self.edges.push(relation, target, source, weight)?;
        }
        Ok(())
    }

    /// The remapped edges and the number of dropped edges.
    pub fn finish(self) -> (EdgeList, u64) {
        (self.edges, self.dropped_edges)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::RelationDictionary;

    fn id_map() -> IdMap<i64> {
        // 10 → 0, 20 → 1 (tombstoned), 30 → 2
        IdMap::new().rebuild(vec![10, 20, 30]).unwrap().rebuild(vec![10, 30]).unwrap()
    }

    fn edges(list: &EdgeList) -> Vec<(usize, DenseId, DenseId)> {
        (0..list.len())
            .map(|i| (list.relations()[i].index(), list.sources()[i], list.targets()[i]))
            .collect()
    }

    #[test]
    fn edges_are_remapped_to_dense_ids() {
        let id_map = id_map();
        let mut relations = RelationDictionary::new();
        let r = relations.intern("r").unwrap();
        let mut intake = EdgeIntake::directed(&id_map, true);
        intake.push(r, Some(&30), Some(&10), 2.5).unwrap();
        let (list, dropped) = intake.finish();
        assert_eq!(edges(&list), [(0, 2, 0)]);
        assert_eq!(list.weights(), Some(&[2.5][..]));
        assert_eq!(dropped, 0);
    }

    #[test]
    fn edges_with_null_unknown_or_tombstoned_endpoints_are_dropped_and_counted() {
        let id_map = id_map();
        let r = RelationDictionary::new().intern("r").unwrap();
        let mut intake = EdgeIntake::undirected(&id_map, false);
        intake.push(r, None, Some(&10), 1.0).unwrap();
        intake.push(r, Some(&10), None, 1.0).unwrap();
        intake.push(r, Some(&10), Some(&99), 1.0).unwrap();
        intake.push(r, Some(&20), Some(&10), 1.0).unwrap();
        let (list, dropped) = intake.finish();
        assert!(list.is_empty());
        assert_eq!(dropped, 4);
    }

    #[test]
    fn undirected_edges_are_emitted_in_both_directions_with_the_same_weight() {
        let id_map = id_map();
        let r = RelationDictionary::new().intern("r").unwrap();
        let mut intake = EdgeIntake::undirected(&id_map, true);
        intake.push(r, Some(&10), Some(&30), 4.0).unwrap();
        let (list, _) = intake.finish();
        assert_eq!(edges(&list), [(0, 0, 2), (0, 2, 0)]);
        assert_eq!(list.weights(), Some(&[4.0, 4.0][..]));
    }

    #[test]
    fn an_undirected_self_loop_is_emitted_once() {
        let id_map = id_map();
        let r = RelationDictionary::new().intern("r").unwrap();
        let mut intake = EdgeIntake::undirected(&id_map, false);
        intake.push(r, Some(&10), Some(&10), 1.0).unwrap();
        let (list, _) = intake.finish();
        assert_eq!(edges(&list), [(0, 0, 0)]);
        assert_eq!(list.weights(), None);
    }
}
