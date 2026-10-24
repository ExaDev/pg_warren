//! Relation groups: the relations whose CSRs one pass over the edge list
//! builds together. One group is fastest, but holds every relation's
//! forward CSR beside the edge list; more groups hold less at once and cost
//! a pass each. The plan takes the fewest groups the budget comfortably
//! allows (`BuildPlan::new`), and at most [`MOST_GROUPS`] of them.

use std::ops::Range;

use super::super::sparse::offsets_bound;

/// The most groups a plan splits relations into, besides one for each
/// relation too large to share one: the finest split, so the most passes.
pub(super) const MOST_GROUPS: u64 = 8;

/// Splits relations `0..relation_edges.len()` into consecutive groups of
/// about `1/parts` of all the relations' forward CSR bytes each: a
/// relation's CSR takes at most its offsets' bound (sparse for few rows,
/// DESIGN.md §5.3) and `entry_bytes` per edge. One part is one group.
pub(super) fn groups(
    relation_edges: &[u64],
    id_space: u64,
    entry_bytes: u64,
    parts: u64,
) -> Vec<Range<usize>> {
    let cost = |edges: u64| {
        offsets_bound(edges, id_space).saturating_add(edges.saturating_mul(entry_bytes))
    };
    let total = relation_edges.iter().fold(0u64, |total, &edges| total.saturating_add(cost(edges)));
    let target = total.div_ceil(parts.max(1)).max(1);
    let mut groups = Vec::new();
    let (mut start, mut held) = (0, 0u64);
    for (relation, &edges) in relation_edges.iter().enumerate() {
        held = held.saturating_add(cost(edges));
        if held >= target {
            groups.push(start..relation + 1);
            (start, held) = (relation + 1, 0);
        }
    }
    if start < relation_edges.len() {
        groups.push(start..relation_edges.len());
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relations_of_similar_size_get_a_group_each_up_to_eight() {
        assert_eq!(groups(&[100; 5], 9, 12, MOST_GROUPS), [0..1, 1..2, 2..3, 3..4, 4..5]);
        assert_eq!(groups(&[], 9, 12, MOST_GROUPS), [] as [Range<usize>; 0]);
    }

    #[test]
    fn one_part_is_one_group() {
        assert_eq!(groups(&[100; 5], 9, 12, 1), vec![Range { start: 0, end: 5 }]);
        assert_eq!(groups(&[100; 4], 9, 12, 2), [0..2, 2..4]);
    }

    #[test]
    fn small_relations_share_a_group_and_large_ones_stand_alone() {
        let edges = [1_000_000, 1, 1, 1, 1, 1, 1, 1, 1, 1];
        assert_eq!(groups(&edges, 9, 12, MOST_GROUPS), [0..1, 1..10]);
        let many = groups(&[10; 100], 9, 12, MOST_GROUPS);
        assert!(many.len() <= 9, "{} groups", many.len());
        assert_eq!(many.iter().map(ExactSizeIterator::len).sum::<usize>(), 100);
    }

    #[test]
    fn relations_of_few_edges_cost_their_edges_not_the_id_space() {
        // One relation from every node, then 199 of two edges each, whose
        // sparse offsets take 32 bytes rather than 8 MB.
        let mut edges = vec![2_000_000];
        edges.extend([2; 199]);
        assert_eq!(groups(&edges, 1_000_000, 12, MOST_GROUPS), [0..1, 1..200]);
    }
}
