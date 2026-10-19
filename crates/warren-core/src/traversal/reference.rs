//! Naive reference traversals over small random graphs, for the property
//! tests: plain edge lists scanned in full, no CSR.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use proptest::prelude::*;

use super::example::relations;
use crate::graph::Direction;
use crate::graph::RelationSelection;
use crate::graph::{AnyCsr, DenseId, EdgeList, GraphCsr, RelationIndex};

/// A small random graph: relations, self-loops, parallel edges, weights and
/// tombstoned nodes, whose edges the build never sees.
#[derive(Debug, Clone)]
pub(crate) struct Scenario {
    pub(crate) id_space: u64,
    pub(crate) relation_count: usize,
    pub(crate) weighted: bool,
    pub(crate) edges: Vec<(usize, DenseId, DenseId, f64)>,
    pub(crate) tombstones: BTreeSet<DenseId>,
}

pub(crate) fn scenarios() -> impl Strategy<Value = Scenario> {
    (1u64..14, 1usize..4, any::<bool>()).prop_flat_map(|(id_space, relation_count, weighted)| {
        let edge = (0..relation_count, 0..id_space, 0..id_space, 0u8..8);
        (prop::collection::vec(edge, 0..40), prop::collection::btree_set(0..id_space, 0..3))
            .prop_map(move |(edges, tombstones)| Scenario {
                id_space,
                relation_count,
                weighted,
                edges: edges
                    .into_iter()
                    .map(|(r, s, t, w)| (r, s, t, weight(weighted, w)))
                    .collect(),
                tombstones,
            })
    })
}

/// An edge weight: 1 if unweighted, else 0–6 or (one time in eight)
/// infinite.
fn weight(weighted: bool, draw: u8) -> f64 {
    match (weighted, draw) {
        (false, _) => 1.0,
        (true, 7) => f64::INFINITY,
        (true, draw) => f64::from(draw),
    }
}

/// A relation selection: `None` for all, else the chosen relations.
pub(crate) fn selections(relation_count: usize) -> impl Strategy<Value = Option<Vec<usize>>> {
    prop::option::of(prop::collection::vec(0..relation_count, 0..3))
}

pub(crate) fn directions() -> impl Strategy<Value = Direction> {
    prop_oneof![Just(Direction::Out), Just(Direction::In), Just(Direction::Both)]
}

impl Scenario {
    /// The graph the build makes of the live edges, with a reverse CSR.
    pub(crate) fn graph(&self) -> GraphCsr {
        GraphCsr::build(self.edge_list(), self.relation_count, self.id_space, true).unwrap()
    }

    /// [`Self::graph`] with `u64` targets, which the build otherwise
    /// chooses only for id spaces of 2³² or more.
    pub(crate) fn wide_graph(&self) -> GraphCsr {
        let graph =
            GraphCsr::build_with::<u64>(self.edge_list(), self.relation_count, self.id_space, true)
                .unwrap();
        assert!(matches!(graph.all_relations().forward(), AnyCsr::U64(_)));
        graph
    }

    fn edge_list(&self) -> EdgeList {
        let indexes = relations(self.relation_count);
        let mut list = EdgeList::new(self.weighted);
        for (r, s, t, w) in self.live_edges() {
            list.push(indexes[r], s, t, w).unwrap();
        }
        list
    }

    /// The relation indexes of `selection`, owned so a test can borrow them.
    pub(crate) fn indexes(&self, selection: &Option<Vec<usize>>) -> Option<Vec<RelationIndex>> {
        let indexes = relations(self.relation_count);
        selection.as_ref().map(|chosen| chosen.iter().map(|&r| indexes[r]).collect())
    }

    /// `indexes` as a relation selection.
    pub(crate) fn selection(indexes: &Option<Vec<RelationIndex>>) -> RelationSelection<'_> {
        match indexes {
            None => RelationSelection::All,
            Some(only) => RelationSelection::Only(only),
        }
    }

    /// The edges whose endpoints are both live.
    pub(crate) fn live_edges(&self) -> impl Iterator<Item = (usize, DenseId, DenseId, f64)> + '_ {
        self.edges
            .iter()
            .copied()
            .filter(|&(_, s, t, _)| !self.tombstones.contains(&s) && !self.tombstones.contains(&t))
    }

    /// One `(from, to, weight)` per live edge of the selected relations, as
    /// traversed in `direction`; parallel edges within a relation are
    /// merged, summing weights if weighted, as the build does.
    pub(crate) fn steps(
        &self,
        direction: Direction,
        selection: &Option<Vec<usize>>,
    ) -> Vec<(DenseId, DenseId, f64)> {
        let mut merged: BTreeMap<(usize, DenseId, DenseId), f64> = BTreeMap::new();
        for (r, s, t, w) in self.live_edges() {
            if selection.as_ref().is_none_or(|chosen| chosen.contains(&r)) {
                let weight = merged.entry((r, s, t)).or_insert(0.0);
                *weight = if self.weighted { *weight + w } else { 1.0 };
            }
        }
        let mut steps = Vec::new();
        for (&(_, s, t), &w) in &merged {
            if direction != Direction::In {
                steps.push((s, t, w));
            }
            if direction != Direction::Out {
                steps.push((t, s, w));
            }
        }
        steps
    }

    /// Each `(from, relation, to)` hop along a live edge of the selected
    /// relations, as traversed in `direction`, once: an edge read both ways
    /// by one relation, or a self-loop, is one hop.
    pub(crate) fn hops(
        &self,
        direction: Direction,
        selection: &Option<Vec<usize>>,
    ) -> BTreeSet<(DenseId, usize, DenseId)> {
        let mut hops = BTreeSet::new();
        for (r, s, t, _) in self.live_edges() {
            if selection.as_ref().is_none_or(|chosen| chosen.contains(&r)) {
                if direction != Direction::In {
                    hops.insert((s, r, t));
                }
                if direction != Direction::Out {
                    hops.insert((t, r, s));
                }
            }
        }
        hops
    }

    /// The BFS depth of every node reachable from `starts` along `steps`,
    /// entering only nodes in `filter` (starts excepted).
    pub(crate) fn depths(
        steps: &[(DenseId, DenseId, f64)],
        starts: &[DenseId],
        filter: Option<&BTreeSet<DenseId>>,
    ) -> BTreeMap<DenseId, u32> {
        let mut depths = BTreeMap::new();
        let mut queue = VecDeque::new();
        for &start in starts {
            if depths.insert(start, 0).is_none() {
                queue.push_back(start);
            }
        }
        while let Some(node) = queue.pop_front() {
            let depth = depths[&node];
            for &(_, to, _) in steps.iter().filter(|&&(from, _, _)| from == node) {
                if !depths.contains_key(&to) && filter.is_none_or(|f| f.contains(&to)) {
                    depths.insert(to, depth + 1);
                    queue.push_back(to);
                }
            }
        }
        depths
    }
}
