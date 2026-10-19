//! How many ends, or how many walks, a walk has (`mode = 'count'`),
//! exactly and without materialising them (DESIGN.md §14 item 15).

use std::ops::ControlFlow;

use super::super::stats::Progress;
use super::super::{Interrupts, TraversalError, Traversed};
use super::plan::WalkPlan;
use super::reach::{reach, Level, Observer, Unlabelled};
use super::tally::tally;

/// The number of distinct ends of `plan` if `distinct_ends`, else the
/// number of walks. Counting walks fails with
/// [`TraversalError::WalkCountOverflow`] past `u64::MAX`.
pub(super) fn count(
    plan: &WalkPlan<'_>,
    distinct_ends: bool,
    interrupts: &mut dyn Interrupts,
) -> Result<Traversed<u64>, TraversalError> {
    let mut progress = Progress::new(interrupts);
    let count = match distinct_ends {
        true => {
            let mut ends = DistinctEnds(0);
            reach::<Unlabelled>(plan, &mut progress, &mut ends)?;
            ends.0
        }
        false => tally(plan, &mut progress)?.iter().try_fold(0u64, |sum, (_, walks)| {
            sum.checked_add(walks).ok_or(TraversalError::WalkCountOverflow)
        })?,
    };
    Ok(progress.traversed(count))
}

/// Counts the ends a search reports, each once.
struct DistinctEnds(u64);

impl Observer<()> for DistinctEnds {
    fn ended(
        &mut self,
        _: Option<Level>,
        _: crate::graph::DenseId,
    ) -> Result<ControlFlow<()>, TraversalError> {
        self.0 += 1;
        Ok(ControlFlow::Continue(()))
    }
}

#[cfg(test)]
mod tests {
    use std::ops::ControlFlow;

    use super::super::example::{knowledge_graph, step, AUTHORED_BY, CITES};
    use super::super::{walk, walk_ends_each, HopRange, Step, WalkMode, WalkOptions, WalkResult};
    use crate::graph::{DenseId, GraphCsr};
    use crate::traversal::example::{directed, graph};
    use crate::traversal::{Direction, TraversalError, Uninterrupted};

    fn count(graph: &GraphCsr, starts: &[DenseId], steps: &[Step], options: WalkOptions) -> u64 {
        match walk(graph, starts, steps, WalkMode::Count, options, &mut Uninterrupted) {
            Ok(found) => match found.result {
                WalkResult::Count(count) => count,
                other => panic!("not a count: {other:?}"),
            },
            Err(error) => panic!("{error}"),
        }
    }

    fn walks() -> WalkOptions {
        WalkOptions { distinct_ends: false, ..WalkOptions::default() }
    }

    fn hops(min: u32, max: u32) -> Step {
        Step {
            relations: None,
            direction: Direction::Out,
            filter: None,
            hops: HopRange::new(min, max).unwrap(),
        }
    }

    #[test]
    fn count_is_the_number_of_distinct_ends_or_of_walks() {
        // 0 → 1 → 3, 0 → 2 → 3
        let diamond = directed(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]);
        assert_eq!(count(&diamond, &[0], &[hops(2, 2)], WalkOptions::default()), 1);
        assert_eq!(count(&diamond, &[0], &[hops(2, 2)], walks()), 2);
        assert_eq!(count(&diamond, &[0], &[hops(1, 2)], WalkOptions::default()), 3);
        assert_eq!(count(&diamond, &[0], &[hops(1, 2)], walks()), 4);
    }

    #[test]
    fn an_edge_in_two_relations_is_two_walks_but_one_both_ways_is_one() {
        let parallel = graph(2, 2, &[(0, 0, 1, 1.0), (1, 0, 1, 1.0)], true);
        assert_eq!(count(&parallel, &[0], &[hops(1, 1)], walks()), 2);
        let mutual = directed(2, &[(0, 1), (1, 0), (0, 0)]);
        let both = Step { direction: Direction::Both, ..hops(1, 1) };
        assert_eq!(count(&mutual, &[0], &[both], walks()), 2, "0 → 1 and the self-loop");
    }

    #[test]
    fn walks_that_split_the_same_hops_differently_between_steps_are_different() {
        let chain = directed(4, &[(0, 1), (1, 2), (2, 3)]);
        // Ends at 2 as (1, 1) and at 3 as (1, 2) or (2, 1).
        assert_eq!(count(&chain, &[0], &[hops(1, 2), hops(1, 2)], walks()), 3);
        assert_eq!(count(&chain, &[0], &[hops(1, 2), hops(1, 2)], WalkOptions::default()), 2);
    }

    #[test]
    fn counting_walks_honours_filters_and_reverse() {
        let graph = knowledge_graph();
        let steps = [step(Some(CITES), 1, 3, None), step(Some(AUTHORED_BY), 1, 1, Some(&[5, 7]))];
        // 0 → 1 → 5, 0 → 4 → 5, 0 → 1 → 2 → 3 → 7
        assert_eq!(count(&graph, &[0], &steps, walks()), 3);
        let reverse = WalkOptions { reverse: true, ..walks() };
        // Into 5: 0 → 1 → 5 and 0 → 4 → 5. Into 7: 2 → 3 → 7, 1 → 2 → 3 → 7
        // and 0 → 1 → 2 → 3 → 7.
        assert_eq!(count(&graph, &[5], &steps, reverse), 2);
        assert_eq!(count(&graph, &[5, 7], &steps, reverse), 5);
    }

    #[test]
    fn a_walk_of_no_steps_counts_its_distinct_starts() {
        let graph = knowledge_graph();
        assert_eq!(count(&graph, &[1, 2, 1], &[], walks()), 2);
    }

    #[test]
    fn a_walk_count_beyond_u64_is_an_error() {
        // Two self-loops on one node: 2^64 walks of 64 hops.
        let loops = graph(1, 2, &[(0, 0, 0, 1.0), (1, 0, 0, 1.0)], true);
        assert_eq!(count(&loops, &[0], &[hops(63, 63)], walks()), 1 << 63);
        assert_eq!(
            walk(&loops, &[0], &[hops(64, 64)], WalkMode::Count, walks(), &mut Uninterrupted)
                .unwrap_err(),
            TraversalError::WalkCountOverflow
        );
    }

    #[test]
    fn ends_without_distinct_repeat_each_end_once_per_walk() {
        let diamond = directed(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]);
        let mut ends = Vec::new();
        let stats =
            walk_ends_each(&diamond, &[0], &[hops(1, 2)], walks(), &mut Uninterrupted, |end| {
                ends.push(end);
                ControlFlow::Continue(())
            })
            .unwrap();
        ends.sort_unstable();
        assert_eq!(ends, [1, 2, 3, 3]);
        assert!(!stats.truncated);
        let capped = WalkOptions { max_results: Some(3), ..walks() };
        let found = walk(&diamond, &[0], &[hops(1, 2)], WalkMode::Ends, capped, &mut Uninterrupted)
            .unwrap();
        assert_eq!(found.result, WalkResult::Ends(vec![1, 2, 3]));
        assert!(found.stats.truncated);
    }
}
