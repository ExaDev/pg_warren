//! The structural diff of two versions of a graph in the same id space
//! (DESIGN.md §7, `warren.diff`): nodes added and removed, edges added and
//! removed per relation and across all relations, and the shift of the
//! out- and in-degree distributions. O(N + M): rows are merged pairwise,
//! never hashed or sorted.

mod degree;
mod edges;
mod nodes;

pub use degree::{DegreeCount, DegreeShift};
pub use edges::{relation_deltas, EdgeDelta, RelationDelta};
pub use nodes::NodeDelta;

use degree::{degree_shift, in_degree_counts, out_degree_counts};
use edges::edge_delta;
use nodes::node_delta;

use super::{shared_dense_ids, GraphVersion, VersionError};
use crate::graph::IdKeys;

/// The structural delta from an earlier version of a graph to a later one.
#[derive(Debug, Clone, PartialEq)]
pub struct VersionDiff {
    /// Live nodes added and removed.
    pub nodes: NodeDelta,
    /// Edges per relation, aligned by name: the later version's relations
    /// in index order, then any found only in the earlier version.
    pub relations: Vec<RelationDelta>,
    /// Edges of the merged (all-relations) CSR.
    pub all_relations: EdgeDelta,
    /// The out-degree distribution over all relations.
    pub out_degree: DegreeShift,
    /// The in-degree distribution over all relations.
    pub in_degree: DegreeShift,
}

/// The structural delta from `before` to `after`, two versions of one graph
/// that share dense ids: `after`'s id space covers `before`'s and assigns
/// the same external id to each of `before`'s dense ids. That fails across
/// a compaction boundary (DESIGN.md §5.2) unless the compaction renumbered
/// nothing.
pub fn diff<K: IdKeys>(
    before: &GraphVersion<'_, K>,
    after: &GraphVersion<'_, K>,
) -> Result<VersionDiff, VersionError> {
    let (earlier, later) = (before.id_map(), after.id_map());
    if later.id_space() < earlier.id_space() {
        return Err(VersionError::IdSpaceShrank {
            before: earlier.id_space(),
            after: later.id_space(),
        });
    }
    shared_dense_ids(earlier, later)?;
    let (forward_before, forward_after) =
        (before.graph().all_relations().forward(), after.graph().all_relations().forward());
    let out_degree = degree_shift(
        &out_degree_counts(earlier, forward_before)?,
        &out_degree_counts(later, forward_after)?,
    );
    let in_degree = degree_shift(
        &in_degree_counts(earlier, forward_before)?,
        &in_degree_counts(later, forward_after)?,
    );
    Ok(VersionDiff {
        nodes: node_delta(earlier, later),
        relations: relation_deltas(
            (before.relations(), before.graph()),
            (after.relations(), after.graph()),
        ),
        all_relations: edge_delta(forward_before, forward_after),
        out_degree,
        in_degree,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::history::{builds, relation_name, Build, EdgeRow};
    use proptest::prelude::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn a_later_version_must_cover_the_earlier_id_space() {
        let v1 = Build::first(&[1, 2], &[], false, false);
        let v2 = v1.next(&[1, 2, 3], &[], false, false);
        assert_eq!(
            diff(&v2.version(), &v1.version()).unwrap_err(),
            VersionError::IdSpaceShrank { before: 3, after: 2 }
        );
    }

    #[test]
    fn versions_of_different_id_spaces_are_refused() {
        let v1 = Build::first(&[1, 2], &[], false, false);
        let other = Build::first(&[1, 3], &[], false, false);
        assert_eq!(
            diff(&v1.version(), &other.version()).unwrap_err(),
            VersionError::IdSpacesDiverge { dense: 1 }
        );
    }

    #[test]
    fn relations_are_aligned_by_name() {
        let v1 = Build::first(&[1, 2], &[(0, 1, 2, 1), (1, 2, 1, 1)], false, true);
        // r1 disappears, r2 appears; the dictionary keeps r1 (append-only).
        let v2 = v1.next(&[1, 2], &[(0, 1, 2, 1), (2, 1, 2, 1)], false, true);
        let delta = diff(&v1.version(), &v2.version()).unwrap();
        let by_name: Vec<(&str, EdgeDelta)> =
            delta.relations.iter().map(|r| (r.name.as_str(), r.edges)).collect();
        assert_eq!(
            by_name,
            [
                ("r0", EdgeDelta { before: 1, after: 1, added: 0, removed: 0 }),
                ("r1", EdgeDelta { before: 1, after: 0, added: 0, removed: 1 }),
                ("r2", EdgeDelta { before: 0, after: 1, added: 1, removed: 0 }),
            ]
        );
        assert_eq!(delta.all_relations, EdgeDelta { before: 2, after: 1, added: 0, removed: 1 });
    }

    #[test]
    fn a_relation_missing_from_the_later_dictionary_is_all_removed() {
        let v1 = Build::first(&[1, 2], &[(1, 1, 2, 1)], false, false);
        let v2 = Build { relations: Default::default(), ..v1.clone() };
        let v2 = v2.next(&[1, 2], &[(0, 2, 1, 1)], false, false);
        let delta = diff(&v1.version(), &v2.version()).unwrap();
        let names: Vec<&str> = delta.relations.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["r0", "r1"]);
        assert_eq!(
            delta.relations[1].edges,
            EdgeDelta { before: 1, after: 0, added: 0, removed: 1 }
        );
    }

    type EdgeSet = BTreeSet<(i64, i64)>;
    type EdgesByRelation = BTreeMap<String, EdgeSet>;

    /// Live nodes and per-relation edges of a build, from its input rows.
    fn naive(nodes: &[i64], rows: &[EdgeRow]) -> (BTreeSet<i64>, EdgesByRelation) {
        let live: BTreeSet<i64> = nodes.iter().copied().collect();
        let mut edges = EdgesByRelation::new();
        for &(relation, source, target, _) in rows {
            let set = edges.entry(relation_name(relation)).or_default();
            if live.contains(&source) && live.contains(&target) {
                set.insert((source, target));
            }
        }
        (live, edges)
    }

    fn delta_of(before: &BTreeSet<(i64, i64)>, after: &BTreeSet<(i64, i64)>) -> EdgeDelta {
        EdgeDelta {
            before: before.len() as u64,
            after: after.len() as u64,
            added: after.difference(before).count() as u64,
            removed: before.difference(after).count() as u64,
        }
    }

    /// Live nodes per degree, for one direction of the merged edges.
    fn degree_counts(
        live: &BTreeSet<i64>,
        merged: &BTreeSet<(i64, i64)>,
        outward: bool,
    ) -> BTreeMap<u64, u64> {
        let mut degrees: BTreeMap<i64, u64> = live.iter().map(|&node| (node, 0)).collect();
        for &(source, target) in merged {
            *degrees.entry(if outward { source } else { target }).or_default() += 1;
        }
        let mut counts = BTreeMap::new();
        for degree in degrees.values() {
            *counts.entry(*degree).or_default() += 1;
        }
        counts
    }

    fn ks(before: &BTreeMap<u64, u64>, after: &BTreeMap<u64, u64>) -> Option<f64> {
        let (n_before, n_after): (u64, u64) = (before.values().sum(), after.values().sum());
        if n_before == 0 || n_after == 0 {
            return None;
        }
        let degrees: BTreeSet<u64> = before.keys().chain(after.keys()).copied().collect();
        let cdf = |counts: &BTreeMap<u64, u64>, at: u64, n: u64| {
            counts.range(..=at).map(|(_, c)| c).sum::<u64>() as f64 / n as f64
        };
        degrees
            .iter()
            .map(|&d| (cdf(before, d, n_before) - cdf(after, d, n_after)).abs())
            .reduce(f64::max)
    }

    proptest! {
        #[test]
        fn diff_matches_naive_set_differences(history in builds(2), weighted in any::<bool>()) {
            let (nodes1, rows1) = &history[0];
            let (nodes2, rows2) = &history[1];
            let v1 = Build::first(nodes1, rows1, weighted, false);
            let v2 = v1.next(nodes2, rows2, weighted, true);
            let delta = diff(&v1.version(), &v2.version()).unwrap();
            let (u1, u2) = (v1.without_merged(), v2.without_merged());
            prop_assert_eq!(&diff(&u1.version(), &u2.version()).unwrap(), &delta);
            prop_assert_eq!(&diff(&v1.version(), &u2.version()).unwrap(), &delta);

            let (live1, edges1) = naive(nodes1, rows1);
            let (live2, edges2) = naive(nodes2, rows2);
            prop_assert_eq!(delta.nodes, NodeDelta {
                before: live1.len() as u64,
                after: live2.len() as u64,
                added: live2.difference(&live1).count() as u64,
                removed: live1.difference(&live2).count() as u64,
            });

            let empty = BTreeSet::new();
            let names: BTreeSet<&String> = edges1.keys().chain(edges2.keys()).collect();
            prop_assert_eq!(delta.relations.len(), names.len());
            for relation in &delta.relations {
                let before = edges1.get(&relation.name).unwrap_or(&empty);
                let after = edges2.get(&relation.name).unwrap_or(&empty);
                prop_assert_eq!(relation.edges, delta_of(before, after));
            }
            let merged = |edges: &EdgesByRelation| -> EdgeSet {
                edges.values().flatten().copied().collect()
            };
            let (merged1, merged2) = (merged(&edges1), merged(&edges2));
            prop_assert_eq!(delta.all_relations, delta_of(&merged1, &merged2));

            for (shift, outward) in [(&delta.out_degree, true), (&delta.in_degree, false)] {
                let before = degree_counts(&live1, &merged1, outward);
                let after = degree_counts(&live2, &merged2, outward);
                let histogram: Vec<DegreeCount> = before.keys().chain(after.keys())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .map(|&degree| DegreeCount {
                        degree,
                        before: before.get(&degree).copied().unwrap_or(0),
                        after: after.get(&degree).copied().unwrap_or(0),
                    })
                    .collect();
                prop_assert_eq!(&shift.histogram, &histogram);
                let mean = |live: &BTreeSet<i64>, merged: &BTreeSet<(i64, i64)>| {
                    (!live.is_empty()).then(|| merged.len() as f64 / live.len() as f64)
                };
                prop_assert_eq!(shift.mean_before, mean(&live1, &merged1));
                prop_assert_eq!(shift.mean_after, mean(&live2, &merged2));
                match (shift.ks_statistic, ks(&before, &after)) {
                    (Some(fast), Some(slow)) => prop_assert!((fast - slow).abs() < 1e-12),
                    (fast, slow) => prop_assert_eq!(fast, slow),
                }
            }
        }
    }
}
