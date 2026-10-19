//! The edge list of a build: edges remapped from external ids to dense ids,
//! with edges whose endpoints are NULL or unknown dropped and counted
//! (DESIGN.md §5.4 step 3).

use super::{memory, DenseId, ExternalId, GraphError, IdMap, RelationIndex, Target, TargetWidth};

/// Edges in dense ids, as parallel arrays, in the order they were pushed.
/// Endpoints are stored at the target width of the id space, so a weighted
/// edge costs 20 bytes below a 2³² id space (DESIGN.md §5.4).
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeList {
    relations: Vec<RelationIndex>,
    endpoints: Endpoints,
    weights: Option<Vec<f64>>,
}

/// The source and target columns, at one width.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Endpoints {
    U32 { sources: Vec<u32>, targets: Vec<u32> },
    U64 { sources: Vec<u64>, targets: Vec<u64> },
}

/// The columns of an edge list, for the CSR build to consume one by one.
pub(crate) struct EdgeColumns {
    pub(crate) relations: Vec<RelationIndex>,
    pub(crate) endpoints: Endpoints,
    pub(crate) weights: Option<Vec<f64>>,
}

impl EdgeList {
    /// An empty edge list for any dense ids, with `u64` endpoints, recording
    /// weights if `weighted`.
    pub fn new(weighted: bool) -> Self {
        Self::for_id_space(u64::MAX, weighted)
    }

    /// An empty edge list whose endpoints lie in `id_space`, stored at the
    /// width the id space implies.
    pub fn for_id_space(id_space: u64, weighted: bool) -> Self {
        let endpoints = match TargetWidth::for_id_space(id_space) {
            TargetWidth::U32 => Endpoints::U32 { sources: Vec::new(), targets: Vec::new() },
            TargetWidth::U64 => Endpoints::U64 { sources: Vec::new(), targets: Vec::new() },
        };
        Self { relations: Vec::new(), endpoints, weights: weighted.then(Vec::new) }
    }

    /// Makes room for exactly `additional` more edges, so a caller that
    /// knows the edge count avoids the geometric growth of [`Self::push`].
    pub fn reserve(&mut self, additional: usize) -> Result<(), GraphError> {
        reserve_exact(&mut self.relations, additional)?;
        match &mut self.endpoints {
            Endpoints::U32 { sources, targets } => {
                reserve_exact(sources, additional)?;
                reserve_exact(targets, additional)
            }
            Endpoints::U64 { sources, targets } => {
                reserve_exact(sources, additional)?;
                reserve_exact(targets, additional)
            }
        }?;
        match &mut self.weights {
            Some(weights) => reserve_exact(weights, additional),
            None => Ok(()),
        }
    }

    /// Appends an edge. `weight` is ignored by an unweighted list.
    pub fn push(
        &mut self,
        relation: RelationIndex,
        source: DenseId,
        target: DenseId,
        weight: f64,
    ) -> Result<(), GraphError> {
        match &mut self.endpoints {
            Endpoints::U32 { sources, targets } => push_endpoints(sources, targets, source, target),
            Endpoints::U64 { sources, targets } => push_endpoints(sources, targets, source, target),
        }?;
        memory::reserve_one(&mut self.relations)?;
        self.relations.push(relation);
        if let Some(weights) = &mut self.weights {
            memory::reserve_one(weights)?;
            weights.push(weight);
        }
        Ok(())
    }

    /// The number of edges.
    pub fn len(&self) -> usize {
        self.relations.len()
    }

    /// Whether the list has no edges.
    pub fn is_empty(&self) -> bool {
        self.relations.is_empty()
    }

    /// The width at which endpoints are stored.
    pub fn width(&self) -> TargetWidth {
        match self.endpoints {
            Endpoints::U32 { .. } => TargetWidth::U32,
            Endpoints::U64 { .. } => TargetWidth::U64,
        }
    }

    /// Each edge as (relation, source, target), in push order.
    pub fn edges(&self) -> impl ExactSizeIterator<Item = (RelationIndex, DenseId, DenseId)> + '_ {
        (0..self.len()).map(move |edge| {
            let (source, target) = match &self.endpoints {
                Endpoints::U32 { sources, targets } => {
                    (sources[edge].dense(), targets[edge].dense())
                }
                Endpoints::U64 { sources, targets } => (sources[edge], targets[edge]),
            };
            (self.relations[edge], source, target)
        })
    }

    /// The weight of each edge, if the list is weighted.
    pub fn weights(&self) -> Option<&[f64]> {
        self.weights.as_deref()
    }

    /// The columns, for the CSR build.
    pub(crate) fn into_columns(self) -> EdgeColumns {
        EdgeColumns { relations: self.relations, endpoints: self.endpoints, weights: self.weights }
    }
}

fn push_endpoints<T: Target>(
    sources: &mut Vec<T>,
    targets: &mut Vec<T>,
    source: DenseId,
    target: DenseId,
) -> Result<(), GraphError> {
    let (Some(narrow_source), Some(narrow_target)) = (T::from_dense(source), T::from_dense(target))
    else {
        return Err(GraphError::EdgeOutsideIdSpace { from: source, to: target });
    };
    memory::reserve_one(sources)?;
    memory::reserve_one(targets)?;
    sources.push(narrow_source);
    targets.push(narrow_target);
    Ok(())
}

fn reserve_exact<V>(column: &mut Vec<V>, additional: usize) -> Result<(), GraphError> {
    column
        .try_reserve_exact(additional)
        .map_err(|_| GraphError::OutOfMemory { elements: column.len().saturating_add(additional) })
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
        let edges = EdgeList::for_id_space(id_map.id_space(), weighted);
        Self { id_map, directed, edges, dropped_edges: 0 }
    }

    /// Makes room for `rows` more edge rows (twice that for an undirected
    /// graph), for a caller that knows the edge count up front.
    pub fn reserve(&mut self, rows: usize) -> Result<(), GraphError> {
        let edges = if self.directed { Some(rows) } else { rows.checked_mul(2) };
        let edges = edges.ok_or(GraphError::OutOfMemory { elements: usize::MAX })?;
        self.edges.reserve(edges)
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
    use crate::graph::{RelationDictionary, TargetWidth};

    fn id_map() -> IdMap<i64> {
        // 10 → 0, 20 → 1 (tombstoned), 30 → 2
        IdMap::new().rebuild(vec![10, 20, 30]).unwrap().rebuild(vec![10, 30]).unwrap()
    }

    fn edges(list: &EdgeList) -> Vec<(usize, DenseId, DenseId)> {
        list.edges().map(|(relation, source, target)| (relation.index(), source, target)).collect()
    }

    #[test]
    fn an_edge_list_stores_endpoints_at_the_width_of_its_id_space() {
        let r = RelationDictionary::new().intern("r").unwrap();
        let mut narrow = EdgeList::for_id_space(1 << 32, false);
        assert_eq!(narrow.width(), TargetWidth::U64);
        narrow = EdgeList::for_id_space(3, false);
        assert_eq!(narrow.width(), TargetWidth::U32);
        narrow.push(r, 2, 1, 1.0).unwrap();
        assert_eq!(edges(&narrow), [(0, 2, 1)]);
        assert_eq!(
            narrow.push(r, 1 << 32, 0, 1.0),
            Err(GraphError::EdgeOutsideIdSpace { from: 1 << 32, to: 0 })
        );
        assert_eq!(narrow.len(), 1);
        assert_eq!(EdgeList::new(false).width(), TargetWidth::U64);
    }

    #[test]
    fn the_intake_uses_the_id_map_width_and_reserves_up_front() {
        let id_map = id_map();
        let r = RelationDictionary::new().intern("r").unwrap();
        let mut intake = EdgeIntake::undirected(&id_map, true);
        assert_eq!(intake.reserve(100), Ok(()));
        assert!(matches!(intake.reserve(usize::MAX / 2), Err(GraphError::OutOfMemory { .. })));
        intake.push(r, Some(&10), Some(&30), 1.0).unwrap();
        let (list, _) = intake.finish();
        assert_eq!(list.width(), TargetWidth::U32);
        assert_eq!(list.len(), 2);
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
