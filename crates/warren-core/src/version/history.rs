//! Test fixture: a history of builds of one graph from external-id rows,
//! run through the same pipeline as `refresh` (DESIGN.md §5.4).

use proptest::prelude::*;

use super::GraphVersion;
use crate::graph::{
    CsrLayout, EdgeIntake, EdgeList, FixedKeys, GraphCsr, IdMap, RelationDictionary,
};

/// One edge row: (relation name index, source, target, weight).
pub(crate) type EdgeRow = (u8, i64, i64, u8);

/// The structures of one built version.
#[derive(Debug, Clone)]
pub(crate) struct Build {
    pub(crate) id_map: IdMap<FixedKeys<i64>>,
    pub(crate) relations: RelationDictionary,
    pub(crate) graph: GraphCsr,
}

impl Build {
    /// The first build of a graph.
    pub(crate) fn first(nodes: &[i64], edges: &[EdgeRow], weighted: bool, reverse: bool) -> Self {
        let empty = Self {
            id_map: IdMap::new(),
            relations: RelationDictionary::new(),
            graph: GraphCsr::build(crate::graph::EdgeList::new(weighted), 0, 0, reverse).unwrap(),
        };
        empty.next(nodes, edges, weighted, reverse)
    }

    /// The next build, starting from this one's id map and dictionary. A
    /// build of an odd number of edge rows is read through shared views of
    /// heap segments, so that the version tests read both owned and shared
    /// buffers.
    pub(crate) fn next(
        &self,
        nodes: &[i64],
        edges: &[EdgeRow],
        weighted: bool,
        reverse: bool,
    ) -> Self {
        let id_map = self.id_map.rebuild(nodes.to_vec()).unwrap();
        let mut relations = self.relations.clone();
        let mut intake = EdgeIntake::directed(&id_map, weighted);
        for &(relation, source, target, weight) in edges {
            let relation = relations.intern(&relation_name(relation)).unwrap();
            intake.push(relation, Some(&source), Some(&target), f64::from(weight)).unwrap();
        }
        let (list, _) = intake.finish();
        let graph = GraphCsr::build(list, relations.len(), id_map.id_space(), reverse).unwrap();
        match edges.len() % 2 {
            0 => Self { id_map, relations, graph },
            _ => Self { id_map: id_map.shared_views(), relations, graph: graph.shared_views() },
        }
    }

    /// This build without its merged CSR: the same relations, rebuilt so
    /// that all-relations reads merge them on the fly.
    pub(crate) fn without_merged(&self) -> Self {
        let graph = &self.graph;
        let mut list = EdgeList::new(graph.all_relations().forward().is_weighted());
        for (position, relation) in graph.relations().iter().enumerate() {
            let index = self.relations.get(&self.relations.names()[position]).unwrap();
            let forward = relation.forward();
            for source in 0..forward.id_space() {
                let weights = forward.neighbour_weights(source);
                for (i, target) in forward.neighbours(source).enumerate() {
                    list.push(index, source, target, weights.map_or(1.0, |w| w[i])).unwrap();
                }
            }
        }
        let layout = CsrLayout { store_merged: false, ..graph.layout() };
        let count = graph.relations().len();
        let id_space = self.id_map.id_space();
        let graph = GraphCsr::build_as(list, count, id_space, layout).unwrap();
        Self { id_map: self.id_map.clone(), relations: self.relations.clone(), graph }
    }

    /// This build as a version view.
    pub(crate) fn version(&self) -> GraphVersion<'_, FixedKeys<i64>> {
        GraphVersion::new(&self.id_map, &self.relations, &self.graph).unwrap()
    }
}

/// The name of relation `index`.
pub(crate) fn relation_name(index: u8) -> String {
    format!("r{index}")
}

/// Random node sets and edge rows over a small external-id range, so that
/// builds share, lose and regain nodes and edges.
pub(crate) fn builds(count: usize) -> impl Strategy<Value = Vec<(Vec<i64>, Vec<EdgeRow>)>> {
    let nodes = prop::collection::vec(0i64..24, 0..20);
    let edges = prop::collection::vec((0u8..3, 0i64..24, 0i64..24, 1u8..5), 0..60);
    prop::collection::vec((nodes, edges), count)
}
