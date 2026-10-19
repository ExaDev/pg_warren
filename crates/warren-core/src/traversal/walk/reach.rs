//! The fast path of a walk: which nodes each step reaches, without counting
//! the walks that reach them (DESIGN.md §9.2).
//!
//! Hops are walks: a node may be reached again at a later depth, so up to a
//! step's `min` hops each depth keeps its own set of nodes. From `min` on a
//! node is admitted at most once per step, which loses nothing: if it was
//! admitted at depth `j ≥ min`, every node it leads to at `k + 1` was
//! already admitted at `j + 1 ≤ max`. Each step keeps its own visited set,
//! so a later step may revisit a node an earlier one reached.

use std::ops::ControlFlow;

use super::super::scratch::Visited;
use super::super::stats::Progress;
use super::super::{Adjacency, TraversalError};
use super::plan::{StepPlan, WalkPlan};
use crate::graph::{memory, DenseId, RelationIndex};

/// A level of a walk: a step, and a depth (hops taken) within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Level {
    pub(super) step: usize,
    pub(super) depth: u32,
}

/// Which rows a search scans, and how it labels a hop.
pub(super) trait Hops {
    type Label: Copy;

    /// Hands each hop from `node` to `hop`, stopping when it breaks.
    fn scan(
        adjacency: &Adjacency<'_>,
        node: DenseId,
        hop: impl FnMut(Self::Label, DenseId) -> Result<ControlFlow<()>, TraversalError>,
    ) -> Result<ControlFlow<()>, TraversalError>;
}

/// Every row entry, unlabelled: the cheapest scan when only nodes matter.
pub(super) struct Unlabelled;

impl Hops for Unlabelled {
    type Label = ();

    fn scan(
        adjacency: &Adjacency<'_>,
        node: DenseId,
        mut hop: impl FnMut((), DenseId) -> Result<ControlFlow<()>, TraversalError>,
    ) -> Result<ControlFlow<()>, TraversalError> {
        for neighbour in adjacency.row_entries(node) {
            if hop((), neighbour)?.is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    }
}

/// Every `(relation, neighbour)` hop once: for reporting walks.
pub(super) struct Labelled;

impl Hops for Labelled {
    type Label = RelationIndex;

    fn scan(
        adjacency: &Adjacency<'_>,
        node: DenseId,
        mut hop: impl FnMut(RelationIndex, DenseId) -> Result<ControlFlow<()>, TraversalError>,
    ) -> Result<ControlFlow<()>, TraversalError> {
        for (relation, neighbour) in adjacency.labelled_neighbours(node) {
            if hop(relation, neighbour)?.is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    }
}

/// What a search reports as it goes.
pub(super) trait Observer<L> {
    /// `node` was admitted at `level` (depth 1 or more) by a hop from `via`.
    fn reached(
        &mut self,
        _level: Level,
        _node: DenseId,
        _via: DenseId,
        _label: L,
    ) -> Result<(), TraversalError> {
        Ok(())
    }

    /// `node` left the step at `level` for the next step.
    fn handed_on(&mut self, _level: Level, _node: DenseId) -> Result<(), TraversalError> {
        Ok(())
    }

    /// `node` ends the walk: it left the last step at `level`, or, if
    /// `None`, it is a start of a walk with no steps.
    fn ended(
        &mut self,
        level: Option<Level>,
        node: DenseId,
    ) -> Result<ControlFlow<()>, TraversalError>;
}

/// Runs `plan`, reporting to `observer`, until the last step is done or
/// `observer` stops the walk. Distinct starts count as visited nodes.
pub(super) fn reach<H: Hops>(
    plan: &WalkPlan<'_>,
    progress: &mut Progress<'_>,
    observer: &mut impl Observer<H::Label>,
) -> Result<(), TraversalError> {
    for _ in &plan.starts {
        progress.node_visited()?;
    }
    if plan.steps.is_empty() {
        for &start in &plan.starts {
            if observer.ended(None, start)?.is_break() {
                break;
            }
        }
        return Ok(());
    }
    let mut inputs = memory::with_capacity(plan.starts.len())?;
    inputs.extend_from_slice(&plan.starts);
    for (index, step) in plan.steps.iter().enumerate() {
        let mut search =
            StepSearch { index, step, last: index + 1 == plan.steps.len(), handed: Vec::new() };
        if search.run::<H>(inputs, progress, observer)?.is_break() {
            break;
        }
        inputs = search.handed;
    }
    Ok(())
}

/// The search of one step.
struct StepSearch<'p, 'w> {
    index: usize,
    step: &'p StepPlan<'w>,
    last: bool,
    /// The nodes handed on to the next step, in the order they left.
    handed: Vec<DenseId>,
}

impl StepSearch<'_, '_> {
    fn run<H: Hops>(
        &mut self,
        mut level: Vec<DenseId>,
        progress: &mut Progress<'_>,
        observer: &mut impl Observer<H::Label>,
    ) -> Result<ControlFlow<()>, TraversalError> {
        let id_space = self.step.adjacency.id_space();
        let (min, max) = (self.step.hops.min(), self.step.hops.max());
        let mut seen = Visited::new(id_space);
        if min == 0 {
            for &node in &level {
                seen.insert(node)?;
                if self.step.exits(node) && self.exit(0, node, observer)?.is_break() {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
        let mut next = Vec::new();
        for depth in 1..=max {
            if level.is_empty() {
                break;
            }
            if depth <= min {
                seen = Visited::new(id_space);
            }
            for &via in &level {
                if !self.step.leaves(via) {
                    continue;
                }
                let flow = H::scan(&self.step.adjacency, via, |label, node| {
                    progress.edge_traversed()?;
                    if !self.step.enters(node)
                        || !self.step.keeps(node, depth)
                        || seen.contains(node)
                    {
                        return Ok(ControlFlow::Continue(()));
                    }
                    seen.insert(node)?;
                    progress.node_visited()?;
                    let level = Level { step: self.index, depth };
                    observer.reached(level, node, via, label)?;
                    if depth >= min
                        && self.step.exits(node)
                        && self.exit(depth, node, observer)?.is_break()
                    {
                        return Ok(ControlFlow::Break(()));
                    }
                    if depth < max && self.step.leaves(node) {
                        memory::reserve_one(&mut next)?;
                        next.push(node);
                    }
                    Ok(ControlFlow::Continue(()))
                })?;
                if flow.is_break() {
                    return Ok(ControlFlow::Break(()));
                }
            }
            std::mem::swap(&mut level, &mut next);
            next.clear();
        }
        Ok(ControlFlow::Continue(()))
    }

    /// `node` leaves the step after `depth` hops: it ends the walk, or is
    /// handed on to the next step.
    fn exit<L>(
        &mut self,
        depth: u32,
        node: DenseId,
        observer: &mut impl Observer<L>,
    ) -> Result<ControlFlow<()>, TraversalError> {
        let level = Level { step: self.index, depth };
        if self.last {
            return observer.ended(Some(level), node);
        }
        observer.handed_on(level, node)?;
        memory::reserve_one(&mut self.handed)?;
        self.handed.push(node);
        Ok(ControlFlow::Continue(()))
    }
}
