use proptest::prelude::*;

use super::*;
use crate::compute::reference::{self, Edges};

/// A dictionary of relations `r0..r<count>`.
fn dictionary(count: usize) -> RelationDictionary {
    let mut dictionary = RelationDictionary::new();
    (0..count).for_each(|r| drop(dictionary.intern(&format!("r{r}"))));
    dictionary
}

fn distance(before: &GraphCsr, after: &GraphCsr) -> EditDistance {
    let (b, a) = (dictionary(before.relations().len()), dictionary(after.relations().len()));
    edit_distance((&b, before), (&a, after))
}

fn named(names: &[&str], changes: &[EdgeSetChange]) -> Vec<RelationChange> {
    names
        .iter()
        .zip(changes)
        .map(|(n, &change)| RelationChange { name: (*n).into(), change })
        .collect()
}

fn change(added: u64, removed: u64, union: u64) -> EdgeSetChange {
    let distance = if union == 0 { 0.0 } else { (added + removed) as f64 / union as f64 };
    EdgeSetChange { added, removed, union, distance }
}

#[test]
fn identical_versions_are_zero_apart() {
    let edges = vec![(0, 0, 1, 1.0), (1, 1, 2, 2.0)];
    let graph = reference::graph(3, 2, &edges, true, false);
    let d = distance(&graph, &graph);
    assert_eq!(d.overall, change(0, 0, 2));
    assert_eq!(d.relations, named(&["r0", "r1"], &[change(0, 0, 1), change(0, 0, 1)]));
}

#[test]
fn changes_are_counted_per_relation_and_overall() {
    // Relation 0 loses 0 → 1 and gains 1 → 0; relation 1 keeps 1 → 2;
    // the later version adds relation 2 and node 3, ignoring weights.
    let before =
        reference::graph(3, 2, &vec![(0, 0, 1, 1.0), (0, 0, 2, 1.0), (1, 1, 2, 2.0)], true, false);
    let after_edges = vec![(0, 0, 2, 9.0), (0, 1, 0, 1.0), (1, 1, 2, 2.0), (2, 3, 0, 1.0)];
    let after = reference::graph(4, 3, &after_edges, true, true);
    let d = distance(&before, &after);
    assert_eq!(
        d.relations,
        named(&["r0", "r1", "r2"], &[change(1, 1, 3), change(0, 0, 1), change(1, 0, 1)])
    );
    assert_eq!(d.overall, change(2, 1, 5));
    let reversed = distance(&after, &before);
    assert_eq!(reversed.overall, change(1, 2, 5));
    assert_eq!(reversed.overall.distance, d.overall.distance);
}

#[test]
fn relations_are_matched_by_name_not_index() {
    // Relation index 1 is `x` before and `y` after: x's edge is removed and
    // y's added, rather than one compared with the other.
    let (mut before_names, mut after_names) =
        (RelationDictionary::new(), RelationDictionary::new());
    for name in ["a", "x"] {
        before_names.intern(name).unwrap();
    }
    for name in ["a", "y"] {
        after_names.intern(name).unwrap();
    }
    let edges = vec![(0, 0, 1, 1.0), (1, 1, 2, 1.0)];
    let (before, after) = (
        reference::graph(3, 2, &edges, false, false),
        reference::graph(3, 2, &edges, false, false),
    );
    let d = edit_distance((&before_names, &before), (&after_names, &after));
    let expected = named(&["a", "y", "x"], &[change(0, 0, 1), change(1, 0, 1), change(0, 1, 1)]);
    assert_eq!(d.relations, expected);
    assert_eq!(d.overall, change(1, 1, 3));
}

#[test]
fn empty_versions_are_zero_apart() {
    let empty = reference::graph(2, 1, &vec![], false, false);
    assert_eq!(distance(&empty, &empty).overall, change(0, 0, 0));
}

/// The perturbation of `base` by `level` of `levels`: the first
/// `level/levels` of its edges removed and as many of the edges of `fresh`
/// not in `base` added, so perturbations nest as the level grows.
fn evolve(base: &Edges, fresh: &Edges, level: usize, levels: usize) -> Edges {
    let key = |&(r, s, t, _): &(usize, u64, u64, f64)| (r, s, t);
    let fresh: Vec<_> = fresh.iter().filter(|e| !base.iter().any(|b| key(b) == key(e))).collect();
    let removed = base.len() * level / levels;
    let added = fresh.len() * level / levels;
    base[removed..].iter().chain(fresh[..added].iter().copied()).copied().collect()
}

fn edges(n: u64, count: usize) -> impl Strategy<Value = Edges> {
    prop::collection::vec((0usize..2, 0..n, 0..n, Just(1.0)), count)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn edit_distance_is_symmetric_and_grows_with_the_perturbation(
        (base, fresh) in (edges(30, 60), edges(30, 60)),
    ) {
        let graph = |edges: &Edges| reference::graph(30, 2, edges, false, false);
        let original = graph(&base);
        let mut previous = 0.0;
        for level in 0..=4 {
            let evolved = graph(&evolve(&base, &fresh, level, 4));
            let d = distance(&original, &evolved);
            prop_assert_eq!(d.overall.distance, distance(&evolved, &original).overall.distance);
            prop_assert!(d.overall.distance >= previous);
            previous = d.overall.distance;
        }
        prop_assert_eq!(distance(&original, &original).overall.distance, 0.0);
    }
}
