//! A naive reference for walks: every match enumerated by brute force over
//! plain hop lists, for the property tests.

use std::collections::BTreeSet;

use proptest::prelude::*;

use super::super::reference::{directions, selections, Scenario};
use super::super::NodeSet;
use super::{HopRange, Step};
use crate::graph::DenseId;
use crate::graph::Direction;

/// A walk as node ids and relation positions.
pub(crate) type NaivePath = (Vec<DenseId>, Vec<usize>);

/// The most matches a reference enumerates before giving up.
const MATCH_LIMIT: usize = 4_000;

/// A step as plain data.
#[derive(Debug, Clone)]
pub(crate) struct NaiveStep {
    pub(crate) selection: Option<Vec<usize>>,
    pub(crate) direction: Direction,
    pub(crate) filter: Option<BTreeSet<DenseId>>,
    pub(crate) min: u32,
    pub(crate) max: u32,
}

pub(crate) fn naive_steps(
    id_space: u64,
    relation_count: usize,
) -> impl Strategy<Value = Vec<NaiveStep>> {
    let step = (
        selections(relation_count),
        directions(),
        prop::option::of(prop::collection::btree_set(0..id_space, 0..=id_space as usize)),
        0u32..3,
        0u32..3,
    )
        .prop_map(|(selection, direction, filter, min, extra)| NaiveStep {
            selection,
            direction,
            filter,
            min,
            max: min + extra,
        });
    prop::collection::vec(step, 0..4)
}

impl NaiveStep {
    /// The step the engine runs.
    pub(crate) fn step(&self, scenario: &Scenario) -> Step {
        Step {
            relations: scenario.indexes(&self.selection),
            direction: self.direction,
            filter: self.filter.as_ref().map(|filter| {
                NodeSet::from_dense_ids(scenario.id_space, filter.iter().copied()).unwrap()
            }),
            hops: HopRange::new(self.min, self.max).unwrap(),
        }
    }

    fn allows(&self, node: DenseId) -> bool {
        self.filter.as_ref().is_none_or(|filter| filter.contains(&node))
    }
}

/// Every match of `steps` from `starts` (each start once), or `None` past
/// [`MATCH_LIMIT`]. A match takes `min..=max` hops per step; every node a
/// hop of a step enters, and the node the step hands on, is in its filter.
pub(crate) fn matches(
    scenario: &Scenario,
    steps: &[NaiveStep],
    starts: &BTreeSet<DenseId>,
) -> Option<Vec<NaivePath>> {
    let hops: Vec<_> =
        steps.iter().map(|step| scenario.hops(step.direction, &step.selection)).collect();
    let mut found = Vec::new();
    for &start in starts {
        let mut path = (vec![start], Vec::new());
        extend(steps, &hops, 0, 0, &mut path, &mut found)?;
    }
    Some(found)
}

fn extend(
    steps: &[NaiveStep],
    hops: &[BTreeSet<(DenseId, usize, DenseId)>],
    index: usize,
    taken: u32,
    path: &mut NaivePath,
    found: &mut Vec<NaivePath>,
) -> Option<()> {
    let Some(step) = steps.get(index) else {
        found.push(path.clone());
        return (found.len() <= MATCH_LIMIT).then_some(());
    };
    let node = *path.0.last().unwrap();
    if taken >= step.min && step.allows(node) {
        extend(steps, hops, index + 1, 0, path, found)?;
    }
    if taken < step.max {
        for &(_, relation, to) in hops[index].iter().filter(|&&(from, _, _)| from == node) {
            if step.allows(to) {
                path.0.push(to);
                path.1.push(relation);
                extend(steps, hops, index, taken + 1, path, found)?;
                path.0.pop();
                path.1.pop();
            }
        }
    }
    Some(())
}
