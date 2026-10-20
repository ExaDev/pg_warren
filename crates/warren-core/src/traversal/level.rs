//! The level-synchronous core shared by every frontier traversal: a
//! frontier, the next frontier and a visited set, expanded one level at a
//! time (DESIGN.md §9.2, §9.3). BFS runs it for one step; a walk segment
//! will run it per step, with that step's adjacency and node filter.
//!
//! A level is pushed (each frontier node's row scanned for unvisited
//! nodes) or, when the search was given the edges read backwards and the
//! frontier is wide, pulled (each unvisited node's reverse row scanned
//! until it meets the frontier): Beamer's direction-optimising BFS
//! (DESIGN.md §10).

use std::ops::ControlFlow;

use super::scratch::Visited;
use super::stats::Progress;
use super::{Adjacency, NodeSet, TraversalError, Visit};
use crate::graph::{memory, DenseId};

/// A frontier is wide enough to pull once it holds more than
/// `1/PULL_FRACTION` of the id space (Beamer's β).
const PULL_FRACTION: u64 = 20;

/// What to do with a node the search is about to admit.
pub(crate) enum Admission {
    /// Admit it and carry on.
    Admit,
    /// Admit it, then stop the search.
    AdmitAndStop,
    /// Stop the search without admitting it.
    Stop,
}

/// What a search follows: an adjacency, the node filter that admits the
/// nodes it reaches, and, if levels may be pulled, the same edges read
/// backwards.
#[derive(Clone, Copy)]
pub(crate) struct Edges<'a, 'g> {
    adjacency: &'a Adjacency<'g>,
    filter: Option<&'a NodeSet>,
    reverse: Option<&'a Adjacency<'g>>,
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
        Ok(Self { adjacency, filter, reverse: None })
    }

    /// The id space of the graph searched.
    pub(crate) fn id_space(&self) -> u64 {
        self.adjacency.id_space()
    }

    /// These edges, letting wide levels be pulled through `reverse` (the
    /// adjacency read in the opposite direction), if given. A pulled level
    /// admits its nodes in ascending dense id order, each via the first
    /// frontier node its reverse rows name, so only searches that promise
    /// neither the order within a depth nor which predecessor they report
    /// pass one.
    pub(crate) fn pulling(self, reverse: Option<&'a Adjacency<'g>>) -> Self {
        Self { reverse, ..self }
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

    /// Expands the current frontier by one level: pulled if `edges` allow
    /// it and the frontier is wide, pushed otherwise.
    pub(crate) fn expand(
        &mut self,
        edges: Edges<'_, '_>,
        progress: &mut Progress<'_>,
        admit: &mut impl FnMut(Visit) -> Result<Admission, TraversalError>,
    ) -> Result<ControlFlow<()>, TraversalError> {
        match edges.reverse {
            Some(reverse) if self.wide(edges.adjacency.id_space()) => {
                self.pull(edges, reverse, progress, admit)
            }
            _ => self.push(edges, progress, admit),
        }
    }

    /// Whether the frontier is wide enough to pull: more than
    /// `1/PULL_FRACTION` of the id space, and more nodes than are left
    /// unvisited. This is Beamer's switch with node counts standing in
    /// for edge counts, so it assumes no great skew in degree between the
    /// frontier and the unvisited nodes.
    fn wide(&self, id_space: u64) -> bool {
        let frontier = self.frontier.len() as u64;
        let unvisited = id_space - self.visited.len();
        frontier * PULL_FRACTION > id_space && frontier > unvisited
    }

    /// Pushes the current frontier one level: scans each frontier node's
    /// row for unvisited nodes, admitting them in the order reached.
    fn push(
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

    /// Pulls the next level: scans each unvisited node's row of `reverse`
    /// until it names a frontier node, admitting the nodes found in
    /// ascending dense id order.
    fn pull(
        &mut self,
        edges: Edges<'_, '_>,
        reverse: &Adjacency<'_>,
        progress: &mut Progress<'_>,
        admit: &mut impl FnMut(Visit) -> Result<Admission, TraversalError>,
    ) -> Result<ControlFlow<()>, TraversalError> {
        let depth = self.depth + 1;
        let id_space = edges.adjacency.id_space();
        let frontier = NodeSet::from_dense_ids(id_space, self.frontier.iter().copied())?;
        self.visited.densify()?;
        for word in 0..frontier.word_count() {
            let mut unvisited = !self.visited.word(word);
            while unvisited != 0 {
                let node =
                    word as u64 * u64::from(u64::BITS) + u64::from(unvisited.trailing_zeros());
                unvisited &= unvisited - 1;
                if node >= id_space || edges.filter.is_some_and(|f| !f.contains(node)) {
                    continue;
                }
                for via in reverse.row_entries(node) {
                    progress.edge_traversed()?;
                    if !frontier.contains(via) {
                        continue;
                    }
                    let visit = Visit { node, depth, via: Some(via) };
                    if self.offer(visit, progress, admit)?.is_break() {
                        return Ok(ControlFlow::Break(()));
                    }
                    break;
                }
            }
        }
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
