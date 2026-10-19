//! Property tests: every walk mode against the naive reference, forwards
//! and in reverse, at both target widths.

use std::collections::BTreeSet;
use std::ops::ControlFlow;

use proptest::prelude::*;

use super::super::reference::{scenarios, Scenario};
use super::reference::{matches, naive_steps, NaivePath, NaiveStep};
use super::{walk, walk_ends_each, Step, WalkMode, WalkOptions, WalkPath, WalkResult};
use crate::graph::{DenseId, GraphCsr};
use crate::interrupts::Uninterrupted;

/// A scenario, a walk over it, and its starts.
fn walks() -> impl Strategy<Value = (Scenario, Vec<NaiveStep>, Vec<DenseId>)> {
    scenarios().prop_flat_map(|scenario| {
        let (n, relation_count) = (scenario.id_space, scenario.relation_count);
        (Just(scenario), naive_steps(n, relation_count), prop::collection::vec(0..n, 0..3))
    })
}

/// What the engine should report: every match, oriented start to end. In
/// reverse, the matches start anywhere and end in `starts`.
fn expected(
    scenario: &Scenario,
    steps: &[NaiveStep],
    starts: &[DenseId],
    reverse: bool,
) -> Option<Vec<NaivePath>> {
    let given: BTreeSet<DenseId> = starts.iter().copied().collect();
    if !reverse {
        return matches(scenario, steps, &given);
    }
    let everywhere: BTreeSet<DenseId> = (0..scenario.id_space).collect();
    let all = matches(scenario, steps, &everywhere)?;
    Some(
        all.into_iter()
            .filter(|(nodes, _)| nodes.last().is_some_and(|end| given.contains(end)))
            .collect(),
    )
}

/// The node a match reports: its end, or in reverse its start.
fn reported(path: &NaivePath, reverse: bool) -> DenseId {
    let nodes = &path.0;
    *if reverse { nodes.first() } else { nodes.last() }.unwrap()
}

fn naive(path: &WalkPath) -> NaivePath {
    (path.nodes.clone(), path.relations.iter().map(|r| r.index()).collect())
}

fn run(
    graph: &GraphCsr,
    starts: &[DenseId],
    steps: &[Step],
    mode: WalkMode,
    options: WalkOptions,
) -> WalkResult {
    walk(graph, starts, steps, mode, options, &mut Uninterrupted).unwrap().result
}

fn sorted<T: Ord>(mut items: Vec<T>) -> Vec<T> {
    items.sort();
    items
}

proptest! {
    #[test]
    fn every_mode_matches_the_naive_reference(
        (scenario, naive_steps, starts) in walks(),
        reverse in any::<bool>(),
        wide in any::<bool>(),
    ) {
        let expected = expected(&scenario, &naive_steps, &starts, reverse);
        prop_assume!(expected.is_some());
        let expected = sorted(expected.unwrap());
        let graph = if wide { scenario.wide_graph() } else { scenario.graph() };
        let steps: Vec<Step> = naive_steps.iter().map(|step| step.step(&scenario)).collect();
        let ends: BTreeSet<DenseId> = expected.iter().map(|path| reported(path, reverse)).collect();
        let every_end = sorted(expected.iter().map(|path| reported(path, reverse)).collect());

        for distinct_ends in [true, false] {
            let options = WalkOptions { distinct_ends, max_results: None, reverse };
            let WalkResult::Ends(found) = run(&graph, &starts, &steps, WalkMode::Ends, options) else {
                panic!("not ends")
            };
            let WalkResult::Count(count) = run(&graph, &starts, &steps, WalkMode::Count, options) else {
                panic!("not a count")
            };
            let WalkResult::Paths(paths) = run(&graph, &starts, &steps, WalkMode::Paths, options) else {
                panic!("not paths")
            };
            let paths: Vec<NaivePath> = paths.iter().map(naive).collect();
            if distinct_ends {
                prop_assert_eq!(found.len(), ends.len(), "an end was reported twice");
                prop_assert_eq!(&found.iter().copied().collect::<BTreeSet<_>>(), &ends);
                prop_assert_eq!(count, ends.len() as u64);
                prop_assert_eq!(paths.len(), ends.len());
                let reached: BTreeSet<DenseId> = paths.iter().map(|p| reported(p, reverse)).collect();
                prop_assert_eq!(&reached, &ends);
                for path in &paths {
                    prop_assert!(expected.binary_search(path).is_ok(), "{:?} is not a walk", path);
                }
            } else {
                prop_assert_eq!(sorted(found), every_end.clone());
                prop_assert_eq!(count, expected.len() as u64);
                prop_assert_eq!(sorted(paths), expected.clone());
            }
        }
    }

    #[test]
    fn max_results_keeps_a_prefix_of_the_rows(
        (scenario, naive_steps, starts) in walks(),
        distinct_ends in any::<bool>(),
        reverse in any::<bool>(),
        max_results in 0u64..5,
    ) {
        prop_assume!(expected(&scenario, &naive_steps, &starts, reverse).is_some());
        let graph = scenario.graph();
        let steps: Vec<Step> = naive_steps.iter().map(|step| step.step(&scenario)).collect();
        let unlimited = WalkOptions { distinct_ends, max_results: None, reverse };
        let limited = WalkOptions { max_results: Some(max_results), ..unlimited };
        for mode in [WalkMode::Ends, WalkMode::Paths] {
            let full = run(&graph, &starts, &steps, mode, unlimited);
            let capped = walk(&graph, &starts, &steps, mode, limited, &mut Uninterrupted).unwrap();
            let (full_len, prefix) = match (&full, &capped.result) {
                (WalkResult::Ends(full), WalkResult::Ends(capped)) => {
                    (full.len(), full.starts_with(capped) && capped.len() == full.len().min(max_results as usize))
                }
                (WalkResult::Paths(full), WalkResult::Paths(capped)) => {
                    (full.len(), full.starts_with(capped) && capped.len() == full.len().min(max_results as usize))
                }
                _ => panic!("mismatched modes"),
            };
            prop_assert!(prefix);
            prop_assert_eq!(capped.stats.truncated, full_len > max_results as usize);
        }
    }

    #[test]
    fn a_filter_prunes_so_the_walk_visits_no_more_nodes(
        (scenario, naive_steps, starts) in walks(),
    ) {
        let graph = scenario.graph();
        let filtered: Vec<Step> = naive_steps.iter().map(|step| step.step(&scenario)).collect();
        let unfiltered: Vec<Step> =
            filtered.iter().cloned().map(|step| Step { filter: None, ..step }).collect();
        let visits = |steps: &[Step]| {
            walk_ends_each(&graph, &starts, steps, WalkOptions::default(), &mut Uninterrupted, |_| {
                ControlFlow::Continue(())
            })
            .unwrap()
            .nodes_visited
        };
        prop_assert!(visits(&filtered) <= visits(&unfiltered));
    }
}
