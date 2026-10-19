//! The CSRs of one graph version: forward and (optionally) reverse per
//! relation, plus the merged all-relations CSR (DESIGN.md §5.3, §5.4 step 4).

use super::{build, transpose::transpose, AnyCsr, Csr, Target};
use crate::graph::{EdgeList, GraphError, RelationIndex};

/// The forward CSR of a relation, and its reverse if the graph builds one.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationCsr {
    forward: AnyCsr,
    reverse: Option<AnyCsr>,
}

impl RelationCsr {
    /// Out-edges: row `v` holds the targets of edges from `v`.
    pub fn forward(&self) -> &AnyCsr {
        &self.forward
    }

    /// In-edges: row `v` holds the sources of edges into `v`, if built.
    pub fn reverse(&self) -> Option<&AnyCsr> {
        self.reverse.as_ref()
    }
}

/// All the CSRs of one graph version.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphCsr {
    relations: Vec<RelationCsr>,
    /// `None` exactly when there is one relation, which then serves as the
    /// all-relations CSR.
    merged: Option<RelationCsr>,
    deduplicated_edges: u64,
}

impl GraphCsr {
    /// Builds the CSRs of `edges`, whose relations index a dictionary of
    /// `relation_count` relations and whose dense ids lie in `id_space`.
    /// Targets are `u32` when `id_space < 2³²`, else `u64`.
    pub fn build(
        edges: EdgeList,
        relation_count: usize,
        id_space: u64,
        build_reverse: bool,
    ) -> Result<Self, GraphError> {
        if id_space <= u64::from(u32::MAX) {
            Self::build_with::<u32>(edges, relation_count, id_space, build_reverse)
        } else {
            Self::build_with::<u64>(edges, relation_count, id_space, build_reverse)
        }
    }

    fn build_with<T: Target>(
        edges: EdgeList,
        relation_count: usize,
        id_space: u64,
        build_reverse: bool,
    ) -> Result<Self, GraphError>
    where
        AnyCsr: From<Csr<T>>,
    {
        let forward = build::forward::<T>(edges, relation_count, id_space)?;
        let with_reverse = |csr: Csr<T>| -> Result<RelationCsr, GraphError> {
            let reverse = match build_reverse {
                true => Some(transpose(&csr)?.into()),
                false => None,
            };
            Ok(RelationCsr { forward: csr.into(), reverse })
        };
        Ok(Self {
            relations: forward.relations.into_iter().map(with_reverse).collect::<Result<_, _>>()?,
            merged: forward.merged.map(with_reverse).transpose()?,
            deduplicated_edges: forward.deduplicated_edges,
        })
    }

    /// The CSRs of each relation, in relation-index order.
    pub fn relations(&self) -> &[RelationCsr] {
        &self.relations
    }

    /// The CSRs of one relation.
    pub fn relation(&self, relation: RelationIndex) -> Option<&RelationCsr> {
        self.relations.get(relation.index())
    }

    /// The all-relations CSRs: the merged CSR, or the only relation's.
    pub fn all_relations(&self) -> &RelationCsr {
        match &self.merged {
            Some(merged) => merged,
            None => &self.relations[0],
        }
    }

    /// The number of parallel edges folded by deduplication.
    pub fn deduplicated_edges(&self) -> u64 {
        self.deduplicated_edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{DenseId, EdgeIntake, IdMap, RelationDictionary};
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    type Naive = BTreeMap<(DenseId, DenseId), f64>;

    fn rows(csr: &AnyCsr) -> Naive {
        let mut edges = Naive::new();
        for source in 0..csr.id_space() {
            let weights = csr.neighbour_weights(source);
            for (i, target) in csr.neighbours(source).enumerate() {
                let weight = weights.map_or(1.0, |w| w[i]);
                edges.insert((source, target), weight);
            }
        }
        edges
    }

    fn reversed(naive: &Naive) -> Naive {
        naive.iter().map(|(&(s, t), &w)| ((t, s), w)).collect()
    }

    fn two_relations() -> (RelationDictionary, RelationIndex, RelationIndex) {
        let mut relations = RelationDictionary::new();
        let a = relations.intern("a").unwrap();
        let b = relations.intern("b").unwrap();
        (relations, a, b)
    }

    #[test]
    fn parallel_edges_are_deduplicated_summing_weights_and_self_loops_kept() {
        let (_, a, _) = two_relations();
        let mut edges = EdgeList::new(true);
        edges.push(a, 2, 0, 1.0).unwrap();
        edges.push(a, 0, 1, 2.0).unwrap();
        edges.push(a, 2, 0, 3.0).unwrap();
        edges.push(a, 1, 1, 4.0).unwrap();
        edges.push(a, 2, 0, 5.0).unwrap();
        let graph = GraphCsr::build(edges, 1, 3, true).unwrap();
        let forward = graph.relations()[0].forward();
        assert_eq!(rows(forward), Naive::from([((0, 1), 2.0), ((1, 1), 4.0), ((2, 0), 9.0)]));
        assert_eq!(graph.deduplicated_edges(), 2);
        assert!(matches!(forward, AnyCsr::U32(_)));
    }

    #[test]
    fn relations_get_their_own_csr_and_the_merged_csr_unites_them() {
        let (_, a, b) = two_relations();
        let mut edges = EdgeList::new(true);
        edges.push(b, 0, 1, 1.0).unwrap();
        edges.push(a, 0, 1, 2.0).unwrap();
        edges.push(a, 1, 0, 3.0).unwrap();
        let graph = GraphCsr::build(edges, 2, 2, true).unwrap();
        assert_eq!(
            rows(graph.relation(a).unwrap().forward()),
            Naive::from([((0, 1), 2.0), ((1, 0), 3.0)])
        );
        assert_eq!(rows(graph.relation(b).unwrap().forward()), Naive::from([((0, 1), 1.0)]));
        let merged = graph.all_relations();
        assert_eq!(rows(merged.forward()), Naive::from([((0, 1), 3.0), ((1, 0), 3.0)]));
        assert_eq!(rows(merged.reverse().unwrap()), Naive::from([((1, 0), 3.0), ((0, 1), 3.0)]));
        assert_eq!(graph.deduplicated_edges(), 0);
    }

    #[test]
    fn a_single_relation_is_its_own_all_relations_csr() {
        let (_, a, _) = two_relations();
        let mut edges = EdgeList::new(false);
        edges.push(a, 0, 1, 1.0).unwrap();
        let graph = GraphCsr::build(edges, 1, 2, false).unwrap();
        assert_eq!(graph.all_relations(), &graph.relations()[0]);
        assert_eq!(graph.all_relations().reverse(), None);
    }

    #[test]
    fn a_graph_without_relations_has_an_empty_all_relations_csr() {
        let graph = GraphCsr::build(EdgeList::new(false), 0, 4, true).unwrap();
        assert!(graph.relations().is_empty());
        assert_eq!(graph.all_relations().forward().id_space(), 4);
        assert_eq!(graph.all_relations().forward().edge_count(), 0);
    }

    #[test]
    fn edges_outside_the_id_space_or_the_dictionary_are_errors() {
        let (_, a, b) = two_relations();
        let mut edges = EdgeList::new(false);
        edges.push(a, 0, 2, 1.0).unwrap();
        assert_eq!(
            GraphCsr::build(edges, 1, 2, false).unwrap_err(),
            GraphError::EdgeOutsideIdSpace { from: 0, to: 2 }
        );
        let mut edges = EdgeList::new(false);
        edges.push(b, 0, 1, 1.0).unwrap();
        assert_eq!(
            GraphCsr::build(edges, 1, 2, false).unwrap_err(),
            GraphError::RelationOutsideDictionary { relation: 1 }
        );
    }

    #[derive(Debug, Clone)]
    struct Scenario {
        relation_count: usize,
        id_space: u64,
        weighted: bool,
        edges: Vec<(usize, DenseId, DenseId, u8)>,
    }

    fn scenarios() -> impl Strategy<Value = Scenario> {
        (1usize..4, 1u64..12, any::<bool>()).prop_flat_map(
            |(relation_count, id_space, weighted)| {
                let edge = (0..relation_count, 0..id_space, 0..id_space, 0u8..8);
                prop::collection::vec(edge, 0..60).prop_map(move |edges| Scenario {
                    relation_count,
                    id_space,
                    weighted,
                    edges,
                })
            },
        )
    }

    proptest! {
        #[test]
        fn build_matches_a_naive_reference(scenario in scenarios()) {
            let mut relations = RelationDictionary::new();
            let indexes: Vec<RelationIndex> = (0..scenario.relation_count)
                .map(|r| relations.intern(&r.to_string()).unwrap())
                .collect();
            let mut edges = EdgeList::new(scenario.weighted);
            let mut per_relation = vec![Naive::new(); scenario.relation_count];
            let mut merged = Naive::new();
            for &(r, source, target, weight) in &scenario.edges {
                let weight = if scenario.weighted { f64::from(weight) } else { 1.0 };
                edges.push(indexes[r], source, target, weight).unwrap();
                let entry = per_relation[r].entry((source, target)).or_insert(0.0);
                *entry = if scenario.weighted { *entry + weight } else { 1.0 };
            }
            for naive in &per_relation {
                for (&key, &weight) in naive {
                    let entry = merged.entry(key).or_insert(0.0);
                    *entry = if scenario.weighted { *entry + weight } else { 1.0 };
                }
            }
            let distinct: usize = per_relation.iter().map(BTreeMap::len).sum();

            let graph = GraphCsr::build(edges, scenario.relation_count, scenario.id_space, true).unwrap();

            prop_assert_eq!(graph.deduplicated_edges(), (scenario.edges.len() - distinct) as u64);
            for (relation, naive) in graph.relations().iter().zip(&per_relation) {
                prop_assert_eq!(relation.forward().id_space(), scenario.id_space);
                prop_assert_eq!(relation.forward().is_weighted(), scenario.weighted);
                prop_assert_eq!(&rows(relation.forward()), naive);
                prop_assert_eq!(rows(relation.reverse().unwrap()), reversed(naive));
            }
            prop_assert_eq!(&rows(graph.all_relations().forward()), &merged);
            prop_assert_eq!(rows(graph.all_relations().reverse().unwrap()), reversed(&merged));
        }

        #[test]
        fn the_pipeline_drops_dangling_edges_and_keeps_the_rest(
            nodes in prop::collection::btree_set(0i64..30, 0..20),
            rows_in in prop::collection::vec((0i64..30, 0i64..30, any::<bool>()), 0..60),
        ) {
            let id_map = IdMap::new().rebuild(nodes.iter().copied().collect()).unwrap();
            let mut relations = RelationDictionary::new();
            let r = relations.intern("r").unwrap();
            let mut intake = EdgeIntake::undirected(&id_map, false);
            let mut naive = Naive::new();
            let mut dangling = 0;
            for &(source, target, null_source) in &rows_in {
                let source_in = (!null_source).then_some(source);
                intake.push(r, source_in.as_ref(), Some(&target), 1.0).unwrap();
                match (source_in.and_then(|s| id_map.dense_id(&s)), id_map.dense_id(&target)) {
                    (Some(s), Some(t)) => {
                        naive.insert((s, t), 1.0);
                        naive.insert((t, s), 1.0);
                    }
                    _ => dangling += 1,
                }
            }
            let (edges, dropped) = intake.finish();
            prop_assert_eq!(dropped, dangling);
            let graph = GraphCsr::build(edges, relations.len(), id_map.id_space(), false).unwrap();
            prop_assert_eq!(rows(graph.all_relations().forward()), naive);
        }
    }
}
