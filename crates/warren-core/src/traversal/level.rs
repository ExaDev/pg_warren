//! The level-synchronous core shared by every frontier traversal: a
//! frontier, the next frontier and a visited set, expanded one level at a
//! time (DESIGN.md §9.2, §9.3). BFS runs it for one step; a walk segment
//! will run it per step, with that step's adjacency and node filter.

use std::ops::ControlFlow;

use super::scratch::Visited;
use super::stats::Progress;
use super::{Adjacency, NodeSet, TraversalError, Visit};
use crate::graph::{memory, DenseId};

/// What to do with a node the search is about to admit.
pub(crate) enum Admission {
    /// Admit it and carry on.
    Admit,
    /// Admit it, then stop the search.
    AdmitAndStop,
    /// Stop the search without admitting it.
    Stop,
}

/// What a search follows: an adjacency, and the node filter that admits
/// the nodes it reaches.
#[derive(Clone, Copy)]
pub(crate) struct Edges<'a, 'g> {
    adjacency: &'a Adjacency<'g>,
    filter: Option<&'a NodeSet>,
}

impl<'a, 'g> Edges<'a, 'g> {
    /// `adjacency`, admitting only nodes in `filter` if given, which must
    /// cover the adjacency's id space.
    pub(crate) fn new(
        adjacency: &'a Adjacency<'g>,
        filter: Option<&'a NodeSet>,
    ) -> Result<Self, TraversalError> {
        if let Some(filter) = filter {
            if filter.id_space() != adjacency.id_space() {
                return Err(TraversalError::NodeFilterIdSpace {
                    expected: adjacency.id_space(),
                    found: filter.id_space(),
                });
            }
        }
        Ok(Self { adjacency, filter })
    }
}

/// A level-synchronous search. It owns its frontiers and visited set but
/// not the edges it follows, which each call passes in (always the same
/// ones), so a caller can suspend it between levels.
pub(crate) struct LevelSearch {
    visited: Visited,
    frontier: Vec<DenseId>,
    next: Vec<DenseId>,
    depth: u32,
}

impl LevelSearch {
    /// A search over `id_space` dense ids that has visited nothing.
    pub(crate) fn new(id_space: u64) -> Self {
        Self { visited: Visited::new(id_space), frontier: Vec::new(), next: Vec::new(), depth: 0 }
    }

    /// Seeds the search with `starts`, then expands it level by level
    /// until the frontier empties, `admit` stops it, or the frontier is at
    /// `max_depth` (unbounded if `None`).
    pub(crate) fn run(
        &mut self,
        edges: Edges<'_, '_>,
        starts: &[DenseId],
        max_depth: Option<u32>,
        progress: &mut Progress<'_>,
        admit: &mut impl FnMut(Visit) -> Result<Admission, TraversalError>,
    ) -> Result<(), TraversalError> {
        let mut flow = self.seed(edges, starts, progress, admit)?;
        while flow.is_continue() && !self.exhausted(max_depth) {
            flow = self.expand(edges, progress, admit)?;
        }
        Ok(())
    }

    /// Whether there is no level left to expand: the frontier is empty or
    /// at `max_depth`.
    pub(crate) fn exhausted(&self, max_depth: Option<u32>) -> bool {
        self.frontier.is_empty() || max_depth.is_some_and(|max_depth| self.depth >= max_depth)
    }

    /// Admits `starts` at depth 0 as the first frontier, dropping repeats.
    /// Starts are always admitted: the node filter applies to the nodes a
    /// search reaches, not to where it begins.
    pub(crate) fn seed(
        &mut self,
        edges: Edges<'_, '_>,
        starts: &[DenseId],
        progress: &mut Progress<'_>,
        admit: &mut impl FnMut(Visit) -> Result<Admission, TraversalError>,
    ) -> Result<ControlFlow<()>, TraversalError> {
        if let Some(&node) = starts.iter().find(|&&node| node >= edges.adjacency.id_space()) {
            return Err(TraversalError::NodeOutsideIdSpace { node });
        }
        for &node in starts {
            if !self.visited.contains(node)
                && self.offer(Visit { node, depth: 0, via: None }, progress, admit)?.is_break()
            {
                return Ok(ControlFlow::Break(()));
            }
        }
        self.advance();
        Ok(ControlFlow::Continue(()))
    }

    /// Expands the current frontier by one level.
    pub(crate) fn expand(
        &mut self,
        edges: Edges<'_, '_>,
        progress: &mut Progress<'_>,
        admit: &mut impl FnMut(Visit) -> Result<Admission, TraversalError>,
    ) -> Result<ControlFlow<()>, TraversalError> {
        let depth = self.depth + 1;
        let frontier = std::mem::take(&mut self.frontier);
        for &via in &frontier {
            for node in edges.adjacency.row_entries(via) {
                progress.edge_traversed()?;
                if self.visited.contains(node) || edges.filter.is_some_and(|f| !f.contains(node)) {
                    continue;
                }
                let visit = Visit { node, depth, via: Some(via) };
                if self.offer(visit, progress, admit)?.is_break() {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
        self.frontier = frontier;
        self.advance();
        self.depth = depth;
        Ok(ControlFlow::Continue(()))
    }

    /// Offers an unvisited node for admission to the next frontier.
    fn offer(
        &mut self,
        visit: Visit,
        progress: &mut Progress<'_>,
        admit: &mut impl FnMut(Visit) -> Result<Admission, TraversalError>,
    ) -> Result<ControlFlow<()>, TraversalError> {
        let admission = admit(visit)?;
        if matches!(admission, Admission::Stop) {
            return Ok(ControlFlow::Break(()));
        }
        progress.node_visited()?;
        self.visited.insert(visit.node)?;
        memory::reserve_one(&mut self.next)?;
        self.next.push(visit.node);
        Ok(match admission {
            Admission::Admit => ControlFlow::Continue(()),
            Admission::AdmitAndStop | Admission::Stop => ControlFlow::Break(()),
        })
    }

    /// Makes the next frontier current, reusing the old one's buffer.
    fn advance(&mut self) {
        std::mem::swap(&mut self.frontier, &mut self.next);
        self.next.clear();
    }
}
