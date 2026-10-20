//! Relation groups: the relations whose CSRs one pass over the edge list
//! builds together. One relation at a time would cost a pass per relation;
//! all at once would hold every relation's CSR beside the edge list.

use std::ops::Range;

/// At most this many groups, so this many passes, besides one for
/// relations too large to share a group.
const GROUPS: u64 = 8;

/// Splits relations `0..relation_edges.len()` into consecutive groups of
/// about an eighth of all the relations' forward CSR bytes each, so a pass
/// builds at most about that: a relation's CSR takes the offsets of the id
/// space (`8·(id_space + 1)` bytes) and `entry_bytes` per edge.
pub(super) fn groups(relation_edges: &[u64], id_space: u64, entry_bytes: u64) -> Vec<Range<usize>> {
    let offsets = id_space.saturating_add(1).saturating_mul(8);
    let cost = |edges: u64| offsets.saturating_add(edges.saturating_mul(entry_bytes));
    let total = relation_edges.iter().fold(0u64, |total, &edges| total.saturating_add(cost(edges)));
    let target = total.div_ceil(GROUPS).max(1);
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
        assert_eq!(groups(&[100; 5], 9, 12), [0..1, 1..2, 2..3, 3..4, 4..5]);
        assert_eq!(groups(&[], 9, 12), [] as [Range<usize>; 0]);
    }

    #[test]
    fn small_relations_share_a_group_and_large_ones_stand_alone() {
        let edges = [1_000_000, 1, 1, 1, 1, 1, 1, 1, 1, 1];
        assert_eq!(groups(&edges, 9, 12), [0..1, 1..10]);
        let many = groups(&[10; 100], 9, 12);
        assert!(many.len() <= 9, "{} groups", many.len());
        assert_eq!(many.iter().map(ExactSizeIterator::len).sum::<usize>(), 100);
    }
}
