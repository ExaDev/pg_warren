//! A BFS that runs one level at a time and owns what it reads, so a caller
//! can suspend it between levels: a set-returning function hands out one
//! level's rows per batch, and SQL `LIMIT` stops the scan by no longer
//! asking for more (DESIGN.md §9.3).

use std::borrow::Borrow;
use std::ops::ControlFlow;

use super::bfs::Results;
use super::level::{Admission, Edges, LevelSearch};
use super::stats::{Progress, Tally};
use super::{Adjacency, Direction, Limits, NodeSet, TraversalError, TraversalStats, Visit};
use crate::graph::{DenseId, GraphCsr, RelationIndex, RelationSelection};
use crate::interrupts::Interrupts;

/// [`bfs`](super::bfs) as a cursor: each [`Self::next_level`] call runs
/// one level (the starts first) and hands its visits to a visitor. The
/// cursor owns its graph handle `G` (for example an `Arc` of a loaded
/// version) and rebuilds its [`Adjacency`] per level, so it borrows
/// nothing between calls. Visits, filter and limits are exactly
/// [`bfs`](super::bfs)'s.
pub struct BfsCursor<G> {
    graph: G,
    direction: Direction,
    relations: Option<Vec<RelationIndex>>,
    filter: Option<NodeSet>,
    max_depth: Option<u32>,
    search: LevelSearch,
    results: Results,
    tally: Tally,
    /// The starts, until the first level seeds the search with them.
    starts: Option<Vec<DenseId>>,
    finished: bool,
}

impl<G: Borrow<GraphCsr>> BfsCursor<G> {
    /// A BFS over `graph` in `direction` along `relations` (all if
    /// `None`) from `starts`, admitting only nodes in `filter` if given.
    /// Checks every argument now, so later calls fail only if interrupted
    /// or out of memory.
    pub fn new(
        graph: G,
        direction: Direction,
        relations: Option<Vec<RelationIndex>>,
        starts: Vec<DenseId>,
        filter: Option<NodeSet>,
        limits: Limits,
    ) -> Result<Self, TraversalError> {
        let id_space = {
            let adjacency = adjacency(graph.borrow(), direction, relations.as_deref())?;
            Edges::new(&adjacency, filter.as_ref())?;
            if let Some(&node) = starts.iter().find(|&&node| node >= adjacency.id_space()) {
                return Err(TraversalError::NodeOutsideIdSpace { node });
            }
            adjacency.id_space()
        };
        Ok(Self {
            graph,
            direction,
            relations,
            filter,
            max_depth: limits.max_depth,
            search: LevelSearch::new(id_space),
            results: Results::new(limits.max_results),
            tally: Tally::default(),
            starts: Some(starts),
            finished: false,
        })
    }

    /// Runs the next level, handing each of its visits to `visitor`, and
    /// returns whether another level may follow. Once it returns `false`
    /// (the search is exhausted, `max_results` was reached, or `visitor`
    /// broke) every later call does nothing and returns `false`.
    pub fn next_level(
        &mut self,
        interrupts: &mut dyn Interrupts,
        mut visitor: impl FnMut(Visit) -> ControlFlow<()>,
    ) -> Result<bool, TraversalError> {
        if self.finished {
            return Ok(false);
        }
        let adjacency = adjacency(self.graph.borrow(), self.direction, self.relations.as_deref())?;
        let edges = Edges::new(&adjacency, self.filter.as_ref())?;
        let mut progress = Progress::resume(interrupts, self.tally);
        let results = &mut self.results;
        let mut admit =
            |visit| -> Result<Admission, TraversalError> { results.admit(visit, &mut visitor) };
        let flow = match self.starts.take() {
            Some(starts) => self.search.seed(edges, &starts, &mut progress, &mut admit)?,
            None => self.search.expand(edges, &mut progress, &mut admit)?,
        };
        if self.results.truncated {
            progress.truncate();
        }
        self.tally = progress.suspend();
        self.finished = flow.is_break() || self.search.exhausted(self.max_depth);
        Ok(!self.finished)
    }

    /// The work done so far.
    pub fn stats(&self) -> TraversalStats {
        self.tally.stats()
    }
}

fn adjacency<'g>(
    graph: &'g GraphCsr,
    direction: Direction,
    relations: Option<&[RelationIndex]>,
) -> Result<Adjacency<'g>, TraversalError> {
    let selection = relations.map_or(RelationSelection::All, RelationSelection::Only);
    Adjacency::new(graph, direction, selection)
}

#[cfg(test)]
mod tests {
    use std::ops::ControlFlow;

    use super::*;
    use crate::graph::RelationSelection;
    use crate::graph::{DenseId, GraphCsr};
    use crate::interrupts::{Cancellation, Uninterrupted};
    use crate::traversal::example::directed;
    use crate::traversal::{bfs, Adjacency, Direction, Limits, NodeSet, TraversalError, Visit};

    // 0 → 1 → 2 → 3, 0 → 4 → 3, 2 → 2, 5 isolated
    fn example() -> GraphCsr {
        directed(6, &[(0, 1), (1, 2), (2, 3), (0, 4), (4, 3), (2, 2)])
    }

    /// Every level of `cursor`, as the visits of each.
    fn levels<G: std::borrow::Borrow<GraphCsr>>(cursor: &mut BfsCursor<G>) -> Vec<Vec<Visit>> {
        let mut levels = Vec::new();
        loop {
            let mut level = Vec::new();
            let more = cursor
                .next_level(&mut Uninterrupted, |visit| {
                    level.push(visit);
                    ControlFlow::Continue(())
                })
                .unwrap();
            if !level.is_empty() {
                levels.push(level);
            }
            if !more {
                return levels;
            }
        }
    }

    fn cursor(
        starts: Vec<DenseId>,
        filter: Option<NodeSet>,
        limits: Limits,
    ) -> BfsCursor<GraphCsr> {
        BfsCursor::new(example(), Direction::Out, None, starts, filter, limits).unwrap()
    }

    #[test]
    fn a_cursor_hands_out_the_bfs_one_level_at_a_time() {
        let mut cursor = cursor(vec![0], None, Limits::default());
        let levels = levels(&mut cursor);
        assert_eq!(levels.iter().map(Vec::len).collect::<Vec<_>>(), [1, 2, 2]);
        let graph = example();
        let adjacency = Adjacency::new(&graph, Direction::Out, RelationSelection::All).unwrap();
        let expected = bfs(&adjacency, &[0], None, Limits::default(), &mut Uninterrupted).unwrap();
        assert_eq!(levels.concat(), expected.result);
        assert_eq!(cursor.stats(), expected.stats);
    }

    #[test]
    fn a_cursor_applies_the_filter_and_the_limits_as_bfs_does() {
        let filter = NodeSet::from_dense_ids(6, [1, 2, 3]).unwrap();
        let limits = Limits { max_depth: Some(2), max_results: Some(2) };
        let mut cursor = cursor(vec![0, 0], Some(filter.clone()), limits);
        let found = levels(&mut cursor).concat();
        let graph = example();
        let adjacency = Adjacency::new(&graph, Direction::Out, RelationSelection::All).unwrap();
        let expected = bfs(&adjacency, &[0], Some(&filter), limits, &mut Uninterrupted).unwrap();
        assert_eq!(found, expected.result);
        assert!(cursor.stats().truncated);
    }

    #[test]
    fn a_visitor_that_breaks_finishes_the_cursor_without_truncating_it() {
        let mut cursor = cursor(vec![0], None, Limits::default());
        let more = cursor.next_level(&mut Uninterrupted, |_| ControlFlow::Break(())).unwrap();
        assert!(!more);
        assert!(!cursor.next_level(&mut Uninterrupted, |_| ControlFlow::Continue(())).unwrap());
        assert_eq!(cursor.stats().nodes_visited, 1);
        assert!(!cursor.stats().truncated);
    }

    #[test]
    fn a_cursor_checks_its_arguments_up_front() {
        let graph = example();
        let unknown = crate::traversal::example::relations(2)[1];
        assert_eq!(
            BfsCursor::new(
                &graph,
                Direction::Out,
                Some(vec![unknown]),
                vec![0],
                None,
                Limits::default()
            )
            .err(),
            Some(TraversalError::UnknownRelation { relation: 1 })
        );
        assert_eq!(
            BfsCursor::new(&graph, Direction::Out, None, vec![6], None, Limits::default()).err(),
            Some(TraversalError::NodeOutsideIdSpace { node: 6 })
        );
        let filter = NodeSet::new(7).unwrap();
        assert_eq!(
            BfsCursor::new(&graph, Direction::Out, None, vec![0], Some(filter), Limits::default())
                .err(),
            Some(TraversalError::NodeFilterIdSpace { expected: 6, found: 7 })
        );
    }

    #[test]
    fn a_raised_cancellation_stops_the_cursor() {
        let mut cursor = cursor(vec![0], None, Limits::default());
        let mut cancellation = Cancellation::new();
        cancellation.cancel();
        assert_eq!(
            cursor.next_level(&mut cancellation, |_| ControlFlow::Continue(())),
            Err(TraversalError::Cancelled)
        );
    }
}

#[cfg(test)]
mod properties {
    use std::ops::ControlFlow;

    use proptest::prelude::*;

    use super::*;
    use crate::interrupts::Uninterrupted;
    use crate::traversal::bfs;
    use crate::traversal::reference::{directions, scenarios, selections, Scenario};

    proptest! {
        #[test]
        fn a_cursor_yields_exactly_the_bfs(
            (scenario, selection, starts, filter) in scenarios().prop_flat_map(|scenario| {
                let n = scenario.id_space;
                let relation_count = scenario.relation_count;
                (
                    Just(scenario),
                    selections(relation_count),
                    prop::collection::vec(0..n, 0..3),
                    prop::option::of(prop::collection::btree_set(0..n, 0..n as usize)),
                )
            }),
            direction in directions(),
            max_depth in prop::option::of(0u32..4),
            max_results in prop::option::of(0u64..8),
        ) {
            let graph = scenario.graph();
            let indexes = scenario.indexes(&selection);
            let filter = filter
                .map(|f| NodeSet::from_dense_ids(scenario.id_space, f.iter().copied()).unwrap());
            let limits = Limits { max_depth, max_results };
            let adjacency = Adjacency::new(&graph, direction, Scenario::selection(&indexes)).unwrap();
            let expected =
                bfs(&adjacency, &starts, filter.as_ref(), limits, &mut Uninterrupted).unwrap();

            let mut cursor =
                BfsCursor::new(&graph, direction, indexes, starts, filter, limits).unwrap();
            let mut found = Vec::new();
            while cursor
                .next_level(&mut Uninterrupted, |visit| {
                    found.push(visit);
                    ControlFlow::Continue(())
                })
                .unwrap()
            {}
            prop_assert_eq!(found, expected.result);
            prop_assert_eq!(cursor.stats(), expected.stats);
        }
    }
}
