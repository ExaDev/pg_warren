use std::collections::BTreeSet;

use proptest::prelude::*;

use super::*;
use crate::compute::{ComputeError, ComputeMemory, Workers};
use crate::graph::{FixedKeys, IdKeys};
use crate::interrupts::{Cancelled, Uninterrupted};
use crate::spectral::EdgeSetChange;
use crate::spectral::{align_ids, RelationChange, Resources};
use crate::version::history::{builds, Build};

/// The `edit` distance from `this` to `other`, aligned by external id.
fn distance(this: &Build, other: &Build) -> EditDistance {
    let workers = Workers::new(1).unwrap();
    let memory = ComputeMemory::new(1 << 20);
    let mut resources =
        Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted };
    let alignment = align_ids(&mut resources, &this.version(), &other.version()).unwrap();
    aligned_edit_distance(&mut Uninterrupted, &this.version(), &other.version(), &alignment)
        .unwrap()
}

fn change(added: u64, removed: u64, union: u64) -> EdgeSetChange {
    let distance = if union == 0 { 0.0 } else { (added + removed) as f64 / union as f64 };
    EdgeSetChange { added, removed, union, distance }
}

fn named(changes: &[(&str, EdgeSetChange)]) -> Vec<RelationChange> {
    changes.iter().map(|&(name, change)| RelationChange { name: name.into(), change }).collect()
}

/// Every (relation name, source, target) edge of `build`, by external id.
fn triples(build: &Build<FixedKeys<i64>>) -> BTreeSet<(String, i64, i64)> {
    let key = |dense| *build.id_map.keys().key(dense).unwrap();
    let names = build.relations.names();
    let mut edges = BTreeSet::new();
    for (name, relation) in names.iter().zip(build.graph.relations()) {
        let forward = relation.forward();
        for source in 0..forward.id_space() {
            for target in forward.neighbours(source) {
                edges.insert((name.clone(), key(source), key(target)));
            }
        }
    }
    edges
}

/// The `edit` distance from `this` to `other` by external id: relations
/// by name, `this`'s first, then those only in `other`.
fn by_external_id(this: &Build, other: &Build) -> EditDistance {
    let (before, after) = (triples(this), triples(other));
    let mut names = this.relations.names().to_vec();
    names.extend(
        other.relations.names().iter().filter(|n| this.relations.get(n).is_none()).cloned(),
    );
    let of = |edges: &BTreeSet<(String, i64, i64)>, name: &str| -> BTreeSet<(i64, i64)> {
        edges.iter().filter(|(n, ..)| n == name).map(|&(_, s, t)| (s, t)).collect()
    };
    let compare = |b: &BTreeSet<(i64, i64)>, a: &BTreeSet<(i64, i64)>| {
        let union = b.union(a).count() as u64;
        change(a.difference(b).count() as u64, b.difference(a).count() as u64, union)
    };
    let relations = names
        .iter()
        .map(|name| RelationChange {
            name: name.clone(),
            change: compare(&of(&before, name), &of(&after, name)),
        })
        .collect();
    let all = |edges: &BTreeSet<(String, i64, i64)>| edges.iter().cloned().collect::<BTreeSet<_>>();
    let (b, a) = (all(&before), all(&after));
    let overall = change(
        a.difference(&b).count() as u64,
        b.difference(&a).count() as u64,
        b.union(&a).count() as u64,
    );
    EditDistance { relations, overall }
}

#[test]
fn edges_are_compared_by_external_id_and_relation_name() {
    // This: 10 → 0, 20 → 1, 30 → 2, relation r0. Other: 5, 20, 30, 40
    // (dense 0..4), relations r1 then r0.
    let this = Build::first(&[10, 20, 30], &[(0, 10, 20, 1)], false, true);
    let other = Build::first(
        &[5, 20, 30, 40],
        &[(1, 5, 40, 1), (0, 20, 30, 1), (0, 40, 20, 1)],
        false,
        true,
    );
    let d = distance(&this, &other);
    assert_eq!(d.relations, named(&[("r0", change(2, 1, 3)), ("r1", change(1, 0, 1))]));
    assert_eq!(d.overall, change(3, 1, 4));
}

#[test]
fn a_version_compared_to_itself_has_no_changes() {
    let v = Build::first(&[1, 2, 3], &[(0, 1, 2, 3), (1, 3, 1, 2), (1, 2, 1, 1)], true, false);
    let d = distance(&v, &v);
    assert_eq!(d.relations, named(&[("r0", change(0, 0, 1)), ("r1", change(0, 0, 2))]));
    assert_eq!(d.overall, change(0, 0, 3));
}

#[test]
fn a_node_tombstoned_in_this_version_shares_no_edge() {
    // 20 is tombstoned in this version, so other's 20 → 30 is added, and
    // this version's relation r1, absent from other, is all removed.
    let this = Build::first(&[10, 20, 30], &[(0, 20, 30, 1)], false, false).next(
        &[10, 30],
        &[(1, 10, 30, 1)],
        false,
        false,
    );
    let other = Build::first(&[20, 30, 40], &[(0, 20, 30, 1)], false, false);
    let d = distance(&this, &other);
    assert_eq!(d.relations, named(&[("r0", change(1, 0, 1)), ("r1", change(0, 1, 1))]));
    assert_eq!(d.overall, change(1, 1, 2));
}

proptest! {
    /// Versions of two graphs compare as their edges by external id do.
    #[test]
    fn versions_compare_as_their_edges_by_external_id(
        history in builds(3),
        weighted in any::<bool>(),
        reverse in any::<bool>(),
    ) {
        let this = Build::first(&history[0].0, &history[0].1, weighted, reverse)
            .next(&history[1].0, &history[1].1, weighted, reverse);
        let other = Build::first(&history[2].0, &history[2].1, !weighted, !reverse);
        prop_assert_eq!(distance(&this, &other), by_external_id(&this, &other));
        prop_assert_eq!(distance(&other, &this), by_external_id(&other, &this));
    }
}

struct Stop;

impl Interrupts for Stop {
    fn check(&mut self) -> Result<(), Cancelled> {
        Err(Cancelled)
    }
}

#[test]
fn comparing_polls_the_interrupts() {
    let v = Build::first(&[1, 2], &[(0, 1, 2, 1)], false, false);
    let workers = Workers::new(1).unwrap();
    let memory = ComputeMemory::new(1 << 20);
    let mut resources =
        Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted };
    let alignment = align_ids(&mut resources, &v.version(), &v.version()).unwrap();
    assert_eq!(
        aligned_edit_distance(&mut Stop, &v.version(), &v.version(), &alignment).unwrap_err(),
        SpectralError::Compute(ComputeError::Cancelled)
    );
}
