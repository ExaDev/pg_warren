//! Walks counted level by level: how many walks reach each node at each
//! depth of each step (DESIGN.md §9.2, §14 item 15).
//!
//! A walk is a sequence of hops, each a `(relation, neighbour)` pair, with
//! the hops of each step its own: the same nodes split differently between
//! steps make different walks. Counting needs no visited set, so it does
//! work at every depth up to each step's `max`.

use super::super::scratch::NodeMap;
use super::super::stats::Progress;
use super::super::TraversalError;
use super::plan::WalkPlan;
use crate::graph::{memory, DenseId};

/// The number of walks reaching each node of one level.
pub(super) struct Tally {
    /// The nodes, in the order they were first reached.
    order: Vec<DenseId>,
    counts: NodeMap<u64>,
}

impl Tally {
    pub(super) fn new(id_space: u64) -> Self {
        Self { order: Vec::new(), counts: NodeMap::new(id_space, 0) }
    }

    /// Adds `walks` walks reaching `node`, returning whether it is new.
    pub(super) fn add(&mut self, node: DenseId, walks: u64) -> Result<bool, TraversalError> {
        let before = self.counts.get(node);
        let after =
            before.unwrap_or(0).checked_add(walks).ok_or(TraversalError::WalkCountOverflow)?;
        self.counts.insert(node, after)?;
        if before.is_none() {
            memory::reserve_one(&mut self.order)?;
            self.order.push(node);
        }
        Ok(before.is_none())
    }

    /// Each node with its walks, in the order first reached.
    pub(super) fn iter(&self) -> impl Iterator<Item = (DenseId, u64)> + '_ {
        self.order.iter().map(|&node| (node, self.counts.get(node).unwrap_or(0)))
    }

    pub(super) fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// Counts the walks of `plan` level by level, and returns the walks ending
/// on each end. Distinct
/// starts count as visited nodes; so does each node new to a level.
pub(super) fn tally(
    plan: &WalkPlan<'_>,
    progress: &mut Progress<'_>,
) -> Result<Tally, TraversalError> {
    let id_space = plan.id_space;
    let mut inputs = Tally::new(id_space);
    for &start in &plan.starts {
        progress.node_visited()?;
        inputs.add(start, 1)?;
    }
    for step in &plan.steps {
        let (min, max) = (step.hops.min(), step.hops.max());
        let mut ends = Tally::new(id_space);
        let mut level = inputs;
        for depth in 0..=max {
            if depth >= min {
                for (node, walks) in level.iter() {
                    if step.exits(node) {
                        ends.add(node, walks)?;
                    }
                }
            }
            let mut next = Tally::new(id_space);
            if depth < max {
                for (via, walks) in level.iter() {
                    if !step.leaves(via) {
                        continue;
                    }
                    for (_, node) in step.adjacency.labelled_neighbours(via) {
                        progress.edge_traversed()?;
                        if step.enters(node)
                            && step.keeps(node, depth + 1)
                            && next.add(node, walks)?
                        {
                            progress.node_visited()?;
                        }
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            level = next;
        }
        inputs = ends;
    }
    Ok(inputs)
}
