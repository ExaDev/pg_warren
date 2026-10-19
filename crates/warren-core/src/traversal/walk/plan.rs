//! A walk as the engine executes it: its steps in execution order, each
//! with the adjacency it reads and the filters it applies, and its starts
//! (DESIGN.md §9.2).
//!
//! Forwards, step `m` admits only nodes in its filter `F_m`, and hands on
//! only nodes in `F_m` (which matters only for a zero-hop step). In
//! reverse the chain runs from its last step to its first with directions
//! flipped, and each filter moves to the other end of its hops: reversed
//! step `m` may hop only *from* nodes in `F_m`, and hands on only nodes in
//! `F_{m-1}` (the filter of the forward step before it), so that every
//! walk it finds is a forward walk read backwards. The starts of a reversed
//! walk are the forward walk's allowed ends, so they must be in `F_last`.

use super::super::scratch::Visited;
use super::super::{Adjacency, NodeSet, TraversalError};
use super::{HopRange, Step};
use crate::graph::{memory, DenseId, GraphCsr};

/// One step as executed.
pub(super) struct StepPlan<'w> {
    pub(super) adjacency: Adjacency<'w>,
    pub(super) hops: HopRange,
    /// Nodes a hop may enter.
    enter: Option<&'w NodeSet>,
    /// Nodes a hop may leave from.
    leave: Option<&'w NodeSet>,
    /// Nodes the step may hand on.
    exit: Option<&'w NodeSet>,
}

impl StepPlan<'_> {
    pub(super) fn enters(&self, node: DenseId) -> bool {
        self.enter.is_none_or(|filter| filter.contains(node))
    }

    pub(super) fn leaves(&self, node: DenseId) -> bool {
        self.leave.is_none_or(|filter| filter.contains(node))
    }

    pub(super) fn exits(&self, node: DenseId) -> bool {
        self.exit.is_none_or(|filter| filter.contains(node))
    }

    /// Whether `node`, entered after `depth` hops, can still be part of a
    /// walk: it can end the step here, or hop on.
    pub(super) fn keeps(&self, node: DenseId, depth: u32) -> bool {
        (depth >= self.hops.min() && self.exits(node))
            || (depth < self.hops.max() && self.leaves(node))
    }
}

/// A whole walk as executed.
pub(super) struct WalkPlan<'w> {
    pub(super) id_space: u64,
    pub(super) steps: Vec<StepPlan<'w>>,
    /// The distinct starts, in the order given, that the first executed
    /// step may begin from.
    pub(super) starts: Vec<DenseId>,
    /// Whether the steps run last to first.
    pub(super) reverse: bool,
}

/// Plans `steps` from `starts` over `graph`, forwards or in reverse.
pub(super) fn plan<'w>(
    graph: &'w GraphCsr,
    starts: &[DenseId],
    steps: &'w [Step],
    reverse: bool,
) -> Result<WalkPlan<'w>, TraversalError> {
    let id_space = graph.all_relations().forward().id_space();
    for filter in steps.iter().filter_map(|step| step.filter.as_ref()) {
        if filter.id_space() != id_space {
            return Err(TraversalError::NodeFilterIdSpace {
                expected: id_space,
                found: filter.id_space(),
            });
        }
    }
    let filter = |m: usize| steps.get(m).and_then(|step| step.filter.as_ref());
    let mut planned = memory::with_capacity(steps.len())?;
    for m in 0..steps.len() {
        let (m, direction, enter, leave, exit) = match reverse {
            false => (m, steps[m].direction, filter(m), None, filter(m)),
            true => {
                let m = steps.len() - 1 - m;
                let before = m.checked_sub(1).and_then(filter);
                (m, steps[m].direction.reversed(), None, filter(m), before)
            }
        };
        let adjacency = Adjacency::new(graph, direction, steps[m].selection())?;
        planned.push(StepPlan { adjacency, hops: steps[m].hops, enter, leave, exit });
    }
    let allowed = match reverse {
        false => None,
        true => steps.len().checked_sub(1).and_then(filter),
    };
    let starts = distinct_starts(id_space, starts, allowed)?;
    Ok(WalkPlan { id_space, steps: planned, starts, reverse })
}

/// `starts` without repeats or nodes outside `allowed`, in the order given.
fn distinct_starts(
    id_space: u64,
    starts: &[DenseId],
    allowed: Option<&NodeSet>,
) -> Result<Vec<DenseId>, TraversalError> {
    if let Some(&node) = starts.iter().find(|&&node| node >= id_space) {
        return Err(TraversalError::NodeOutsideIdSpace { node });
    }
    let mut seen = Visited::new(id_space);
    let mut distinct = Vec::new();
    for &node in starts {
        if allowed.is_none_or(|filter| filter.contains(node)) && seen.insert(node)? {
            memory::reserve_one(&mut distinct)?;
            distinct.push(node);
        }
    }
    Ok(distinct)
}
