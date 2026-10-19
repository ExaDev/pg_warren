//! The end frontier of a walk (`mode = 'ends'`).

use std::ops::ControlFlow;

use super::super::stats::Progress;
use super::super::{Interrupts, TraversalError, TraversalStats};
use super::plan::plan;
use super::reach::{reach, Level, Observer, Unlabelled};
use super::{RowLimit, Step, WalkOptions};
use crate::graph::{DenseId, GraphCsr};

/// The ends of every walk along `steps` from `starts` (or, in reverse, the
/// starts of every walk ending in `starts`), each handed to `visitor` as it
/// is found, once each; returning [`ControlFlow::Break`] stops the walk
/// after that end, which is not truncation. Stopping because another end
/// would exceed `options.max_results` is.
pub fn walk_ends_each(
    graph: &GraphCsr,
    starts: &[DenseId],
    steps: &[Step],
    options: WalkOptions,
    interrupts: &mut dyn Interrupts,
    visitor: impl FnMut(DenseId) -> ControlFlow<()>,
) -> Result<TraversalStats, TraversalError> {
    let plan = plan(graph, starts, steps, options.reverse)?;
    let mut progress = Progress::new(interrupts);
    let mut ends = Ends { visitor, limit: RowLimit::new(options.max_results) };
    reach::<Unlabelled>(&plan, &mut progress, &mut ends)?;
    ends.limit.record(&mut progress);
    Ok(progress.finish())
}

/// Hands each end to the visitor, within the row limit.
struct Ends<V> {
    visitor: V,
    limit: RowLimit,
}

impl<V: FnMut(DenseId) -> ControlFlow<()>> Observer<()> for Ends<V> {
    fn ended(
        &mut self,
        _: Option<Level>,
        node: DenseId,
    ) -> Result<ControlFlow<()>, TraversalError> {
        Ok(self.limit.offer(|| (self.visitor)(node)))
    }
}

#[cfg(test)]
mod tests {
    use std::ops::ControlFlow;

    use super::super::{walk, walk_ends_each, HopRange, Step, WalkMode, WalkOptions, WalkResult};
    use crate::graph::{DenseId, GraphCsr};
    use crate::traversal::example::{directed, graph, relations};
    use crate::traversal::{
        Direction, NodeSet, TraversalError, TraversalStats, Traversed, Uninterrupted,
    };

    const CITES: usize = 0;
    const AUTHORED_BY: usize = 1;
    const AFFILIATED_WITH: usize = 2;

    /// Papers 0–4, people 5–7, institutions 8–9.
    /// cites: 0 → 1 → 2 → 3, 0 → 4; authored_by: 1 → 5, 2 → 6, 3 → 7,
    /// 4 → 5; affiliated_with: 5 → 8, 6 → 9, 7 → 8.
    fn knowledge_graph() -> GraphCsr {
        let edges = [
            (CITES, 0, 1),
            (CITES, 1, 2),
            (CITES, 2, 3),
            (CITES, 0, 4),
            (AUTHORED_BY, 1, 5),
            (AUTHORED_BY, 2, 6),
            (AUTHORED_BY, 3, 7),
            (AUTHORED_BY, 4, 5),
            (AFFILIATED_WITH, 5, 8),
            (AFFILIATED_WITH, 6, 9),
            (AFFILIATED_WITH, 7, 8),
        ];
        let edges: Vec<_> = edges.iter().map(|&(r, s, t)| (r, s, t, 1.0)).collect();
        graph(10, 3, &edges, true)
    }

    pub(crate) fn step(
        relation: Option<usize>,
        min: u32,
        max: u32,
        filter: Option<&[DenseId]>,
    ) -> Step {
        Step {
            relations: relation.map(|r| vec![relations(3)[r]]),
            direction: Direction::Out,
            filter: filter.map(|f| NodeSet::from_dense_ids(10, f.iter().copied()).unwrap()),
            hops: HopRange::new(min, max).unwrap(),
        }
    }

    fn ends_of(
        graph: &GraphCsr,
        starts: &[DenseId],
        steps: &[Step],
        options: WalkOptions,
    ) -> Result<Traversed<Vec<DenseId>>, TraversalError> {
        let found = walk(graph, starts, steps, WalkMode::Ends, options, &mut Uninterrupted)?;
        let WalkResult::Ends(ends) = found.result;
        Ok(Traversed { result: ends, stats: found.stats })
    }

    fn ends(graph: &GraphCsr, starts: &[DenseId], steps: &[Step]) -> Vec<DenseId> {
        let mut ends = ends_of(graph, starts, steps, WalkOptions::default()).unwrap().result;
        ends.sort_unstable();
        ends
    }

    #[test]
    fn a_one_hop_step_ends_at_the_neighbours() {
        let graph = knowledge_graph();
        assert_eq!(ends(&graph, &[0], &[step(Some(CITES), 1, 1, None)]), [1, 4]);
        assert_eq!(ends(&graph, &[0], &[step(None, 1, 1, None)]), [1, 4]);
    }

    #[test]
    fn a_chain_of_typed_steps_follows_each_relation_in_turn() {
        let graph = knowledge_graph();
        let steps = [
            step(Some(CITES), 1, 3, None),
            step(Some(AUTHORED_BY), 1, 1, None),
            step(Some(AFFILIATED_WITH), 1, 1, None),
        ];
        assert_eq!(ends(&graph, &[0], &steps), [8, 9]);
    }

    #[test]
    fn filters_restrict_the_nodes_each_step_reaches() {
        let graph = knowledge_graph();
        let steps = [
            step(Some(CITES), 1, 3, None),
            step(Some(AUTHORED_BY), 1, 1, Some(&[5, 7])),
            step(Some(AFFILIATED_WITH), 1, 1, None),
        ];
        assert_eq!(ends(&graph, &[0], &steps), [8]);
    }

    #[test]
    fn a_filter_prunes_the_hops_inside_a_step() {
        let graph = knowledge_graph();
        // Without 2, the citation chain stops at 1; 3 is out of reach.
        let pruned = [step(Some(CITES), 1, 3, Some(&[1, 3, 4]))];
        assert_eq!(ends(&graph, &[0], &pruned), [1, 4]);
    }

    #[test]
    fn a_step_ends_at_every_depth_in_its_hop_range() {
        let graph = knowledge_graph();
        assert_eq!(ends(&graph, &[0], &[step(Some(CITES), 2, 3, None)]), [2, 3]);
        assert_eq!(ends(&graph, &[0], &[step(Some(CITES), 3, 3, None)]), [3]);
    }

    #[test]
    fn hops_are_walks_so_a_node_closer_than_min_still_ends_at_min() {
        // 0 → 1 → 2 and 0 → 2: 2 is one hop away but also two.
        let graph = directed(3, &[(0, 1), (1, 2), (0, 2)]);
        let two = Step { hops: HopRange::exactly(2).unwrap(), ..step(None, 1, 1, None) };
        let mut found = ends_of(&graph, &[0], &[two], WalkOptions::default()).unwrap().result;
        found.sort_unstable();
        assert_eq!(found, [2]);
        // 0 → 1 → 0: the start is two hops from itself.
        let cycle = directed(2, &[(0, 1), (1, 0)]);
        let two = Step { hops: HopRange::exactly(2).unwrap(), ..step(None, 1, 1, None) };
        assert_eq!(ends_of(&cycle, &[0], &[two], WalkOptions::default()).unwrap().result, [0]);
    }

    #[test]
    fn a_later_step_may_revisit_a_node_an_earlier_step_reached() {
        let graph = directed(2, &[(0, 1), (1, 0)]);
        let out = Step { filter: None, ..step(None, 1, 1, None) };
        let found = ends_of(&graph, &[0], &[out.clone(), out], WalkOptions::default()).unwrap();
        assert_eq!(found.result, [0]);
    }

    #[test]
    fn zero_hops_hands_on_the_step_s_own_start_if_the_filter_allows() {
        let graph = knowledge_graph();
        assert_eq!(ends(&graph, &[0], &[step(Some(CITES), 0, 1, None)]), [0, 1, 4]);
        assert_eq!(ends(&graph, &[0], &[step(Some(CITES), 0, 1, Some(&[1, 4]))]), [1, 4]);
        assert_eq!(ends(&graph, &[0, 1], &[step(Some(CITES), 0, 0, Some(&[1]))]), [1]);
    }

    #[test]
    fn a_walk_of_no_steps_ends_where_it_starts() {
        let graph = knowledge_graph();
        let found = ends_of(&graph, &[3, 1, 3], &[], WalkOptions::default()).unwrap();
        assert_eq!(found.result, [3, 1]);
        assert_eq!(found.stats.nodes_visited, 2);
    }

    #[test]
    fn repeated_starts_count_once_and_each_end_is_reported_once() {
        let graph = knowledge_graph();
        let steps = [step(Some(CITES), 1, 1, None), step(Some(AUTHORED_BY), 1, 1, None)];
        let found = ends_of(&graph, &[0, 0], &steps, WalkOptions::default()).unwrap().result;
        assert_eq!(found, [5]);
    }

    #[test]
    fn a_start_outside_the_id_space_is_an_error() {
        let graph = knowledge_graph();
        assert_eq!(
            ends_of(&graph, &[10], &[step(None, 1, 1, None)], WalkOptions::default()).unwrap_err(),
            TraversalError::NodeOutsideIdSpace { node: 10 }
        );
    }

    #[test]
    fn a_filter_over_another_id_space_is_an_error() {
        let graph = knowledge_graph();
        let wrong = Step { filter: Some(NodeSet::new(11).unwrap()), ..step(None, 1, 1, None) };
        assert_eq!(
            ends_of(&graph, &[0], &[wrong], WalkOptions::default()).unwrap_err(),
            TraversalError::NodeFilterIdSpace { expected: 10, found: 11 }
        );
    }

    #[test]
    fn a_step_in_or_both_needs_the_reverse_csr() {
        let graph = graph(2, 1, &[(0, 0, 1, 1.0)], false);
        let into = Step { direction: Direction::In, ..step(None, 1, 1, None) };
        assert_eq!(
            ends_of(&graph, &[0], &[into], WalkOptions::default()).unwrap_err(),
            TraversalError::ReverseCsrMissing
        );
    }

    #[test]
    fn stats_cover_every_step_and_filters_mean_fewer_visits() {
        let graph = knowledge_graph();
        let mut steps = vec![
            step(Some(CITES), 1, 3, None),
            step(Some(AUTHORED_BY), 1, 1, Some(&[7])),
            step(Some(AFFILIATED_WITH), 1, 1, None),
        ];
        let filtered = ends_of(&graph, &[0], &steps, WalkOptions::default()).unwrap();
        // start 0; papers 1, 4, 2, 3; person 7; institution 8
        assert_eq!(
            filtered.stats,
            TraversalStats { nodes_visited: 7, edges_traversed: 9, truncated: false }
        );
        steps[1].filter = None;
        let unfiltered = ends_of(&graph, &[0], &steps, WalkOptions::default()).unwrap();
        assert_ne!(unfiltered.result, filtered.result);
        assert!(unfiltered.stats.nodes_visited > filtered.stats.nodes_visited);
    }

    #[test]
    fn max_results_stops_the_walk_and_marks_it_truncated() {
        let graph = knowledge_graph();
        let steps = [step(Some(CITES), 1, 3, None)];
        let capped = WalkOptions { max_results: Some(2), ..WalkOptions::default() };
        let found = ends_of(&graph, &[0], &steps, capped).unwrap();
        assert_eq!(found.result.len(), 2);
        assert!(found.stats.truncated);
        let roomy = WalkOptions { max_results: Some(4), ..WalkOptions::default() };
        let found = ends_of(&graph, &[0], &steps, roomy).unwrap();
        assert_eq!(found.result.len(), 4);
        assert!(!found.stats.truncated);
    }

    #[test]
    fn the_streaming_form_stops_when_the_visitor_breaks_without_truncating() {
        let graph = knowledge_graph();
        let steps = [step(Some(CITES), 1, 3, None)];
        let mut seen = Vec::new();
        let stats = walk_ends_each(
            &graph,
            &[0],
            &steps,
            WalkOptions::default(),
            &mut Uninterrupted,
            |end| {
                seen.push(end);
                ControlFlow::Break(())
            },
        )
        .unwrap();
        assert_eq!(seen.len(), 1);
        assert!(!stats.truncated);
    }

    #[test]
    fn a_raised_cancellation_stops_the_walk() {
        let graph = knowledge_graph();
        let mut cancelled = crate::traversal::Cancellation::new();
        cancelled.cancel();
        let steps = [step(None, 1, 1, None)];
        assert_eq!(
            walk(&graph, &[0], &steps, WalkMode::Ends, WalkOptions::default(), &mut cancelled)
                .unwrap_err(),
            TraversalError::Cancelled
        );
    }
}
