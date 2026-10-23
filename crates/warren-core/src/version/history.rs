//! Test fixture: a history of builds of one graph from external-id rows,
//! run through the same pipeline as `refresh` (DESIGN.md §5.4).

use proptest::prelude::*;

use super::GraphVersion;
use crate::graph::{
    BuildMemory, CsrLayout, EdgeIntake, EdgeList, FixedKeys, GraphCsr, IdKeys, IdMap,
    RelationDictionary, TextIds, TextKeys,
};

/// One edge row: (relation name index, source, target, weight).
pub(crate) type EdgeRow = (u8, i64, i64, u8);

/// How the fixture's integer nodes become the keys of a key storage.
pub(crate) trait Keying {
    /// The key storage.
    type Keys: IdKeys;

    /// The id map of the next build of `id_map` over `nodes`.
    fn rebuild(&self, id_map: &IdMap<Self::Keys>, nodes: &[i64]) -> IdMap<Self::Keys>;

    /// Calls `f` with the key of `node`.
    fn with_key<R>(&self, node: i64, f: impl FnOnce(&<Self::Keys as IdKeys>::Key) -> R) -> R;

    /// The same id map with every buffer a shared view of a heap segment.
    fn shared_views(id_map: &IdMap<Self::Keys>) -> IdMap<Self::Keys>;
}

/// Nodes as `int8` ids.
pub(crate) struct Ints;

impl Keying for Ints {
    type Keys = FixedKeys<i64>;

    fn rebuild(&self, id_map: &IdMap<FixedKeys<i64>>, nodes: &[i64]) -> IdMap<FixedKeys<i64>> {
        id_map.rebuild(nodes.to_vec()).unwrap()
    }

    fn with_key<R>(&self, node: i64, f: impl FnOnce(&i64) -> R) -> R {
        f(&node)
    }

    fn shared_views(id_map: &IdMap<FixedKeys<i64>>) -> IdMap<FixedKeys<i64>> {
        id_map.shared_views()
    }
}

/// Node `n` as the `n`th of some text ids, in byte order: the
/// order-preserving bijection from each text id to its rank.
#[derive(Debug, Clone)]
pub(crate) struct Ranks(pub(crate) Vec<Vec<u8>>);

impl Keying for Ranks {
    type Keys = TextKeys;

    fn rebuild(&self, id_map: &IdMap<TextKeys>, nodes: &[i64]) -> IdMap<TextKeys> {
        let mut ids = TextIds::new(&BuildMemory::unlimited());
        nodes.iter().for_each(|&node| self.with_key(node, |id| ids.push(id)).unwrap());
        id_map.rebuild_within(ids).unwrap().0
    }

    fn with_key<R>(&self, node: i64, f: impl FnOnce(&[u8]) -> R) -> R {
        f(&self.0[node as usize])
    }

    fn shared_views(id_map: &IdMap<TextKeys>) -> IdMap<TextKeys> {
        id_map.shared_views()
    }
}

/// The structures of one built version.
#[derive(Debug, Clone)]
pub(crate) struct Build<K = FixedKeys<i64>> {
    pub(crate) id_map: IdMap<K>,
    pub(crate) relations: RelationDictionary,
    pub(crate) graph: GraphCsr,
}

impl Build {
    /// The first build of a graph.
    pub(crate) fn first(nodes: &[i64], edges: &[EdgeRow], weighted: bool, reverse: bool) -> Self {
        Self::first_with(&Ints, nodes, edges, weighted, reverse)
    }

    /// The next build, starting from this one's id map and dictionary.
    pub(crate) fn next(
        &self,
        nodes: &[i64],
        edges: &[EdgeRow],
        weighted: bool,
        reverse: bool,
    ) -> Self {
        self.next_with(&Ints, nodes, edges, weighted, reverse)
    }
}

impl<K: IdKeys + Clone> Build<K> {
    /// The first build of a graph whose nodes are keyed by `keying`.
    pub(crate) fn first_with(
        keying: &impl Keying<Keys = K>,
        nodes: &[i64],
        edges: &[EdgeRow],
        weighted: bool,
        reverse: bool,
    ) -> Self {
        let empty = Self {
            id_map: IdMap::new(),
            relations: RelationDictionary::new(),
            graph: GraphCsr::build(crate::graph::EdgeList::new(weighted), 0, 0, reverse).unwrap(),
        };
        empty.next_with(keying, nodes, edges, weighted, reverse)
    }

    /// The next build, starting from this one's id map and dictionary,
    /// with nodes keyed by `keying`. A build of an odd number of edge rows
    /// is read through shared views of heap segments, so that the version
    /// tests read both owned and shared buffers.
    pub(crate) fn next_with<Y: Keying<Keys = K>>(
        &self,
        keying: &Y,
        nodes: &[i64],
        edges: &[EdgeRow],
        weighted: bool,
        reverse: bool,
    ) -> Self {
        let id_map = keying.rebuild(&self.id_map, nodes);
        let mut relations = self.relations.clone();
        let mut intake = EdgeIntake::directed(&id_map, weighted);
        for &(relation, source, target, weight) in edges {
            let relation = relations.intern(&relation_name(relation)).unwrap();
            keying
                .with_key(source, |source| {
                    keying.with_key(target, |target| {
                        intake.push(relation, Some(source), Some(target), f64::from(weight))
                    })
                })
                .unwrap();
        }
        let (list, _) = intake.finish();
        let graph = GraphCsr::build(list, relations.len(), id_map.id_space(), reverse).unwrap();
        match edges.len() % 2 {
            0 => Self { id_map, relations, graph },
            _ => Self { id_map: Y::shared_views(&id_map), relations, graph: graph.shared_views() },
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
    pub(crate) fn version(&self) -> GraphVersion<'_, K> {
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

/// 24 distinct text ids in byte order, drawn from every id of up to three
/// bytes of `a`, `b`, 0 and 255: short, sharing prefixes, the empty id
/// among them.
pub(crate) fn ranks() -> impl Strategy<Value = Ranks> {
    let mut ids = vec![Vec::new()];
    for length in 1..=3 {
        let longer: Vec<Vec<u8>> =
            ids.iter().filter(|id| id.len() == length - 1).cloned().collect();
        for id in longer {
            ids.extend(b"ab\x00\xff".iter().map(|&byte| [id.as_slice(), &[byte]].concat()));
        }
    }
    ids.sort();
    prop::sample::subsequence(ids, 24).prop_map(Ranks)
}
