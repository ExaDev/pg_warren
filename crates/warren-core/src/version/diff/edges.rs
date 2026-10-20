//! The edge delta of a diff: edges added and removed, by merging the two
//! versions' sorted CSR rows.

use std::cmp::Ordering;

use crate::graph::RelationDictionary;
use crate::graph::{AnyCsr, CsrRows, GraphCsr, RelationCsr, RowNeighbours};

/// Edges before and after, and how many were added and removed. An edge is
/// a (source, target) pair; a change of weight alone is not counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EdgeDelta {
    /// Edges in the earlier version.
    pub before: u64,
    /// Edges in the later version.
    pub after: u64,
    /// Edges in the later version but not the earlier.
    pub added: u64,
    /// Edges in the earlier version but not the later.
    pub removed: u64,
}

impl EdgeDelta {
    /// Every edge of `csr` added (the relation is new in the later version).
    pub(super) fn all_added(csr: &AnyCsr) -> Self {
        Self { after: csr.edge_count(), added: csr.edge_count(), ..Self::default() }
    }

    /// Every edge of `csr` removed (the relation is gone from the later
    /// version).
    pub(super) fn all_removed(csr: &AnyCsr) -> Self {
        Self { before: csr.edge_count(), removed: csr.edge_count(), ..Self::default() }
    }

    /// The edge delta between two forward CSRs of one relation in the same
    /// id space, either of which may be missing (the relation is absent from
    /// that version). Also the `edit` distance's merge (DESIGN.md §8).
    pub(crate) fn between(before: Option<&AnyCsr>, after: Option<&AnyCsr>) -> Self {
        match (before, after) {
            (Some(before), Some(after)) => edge_delta(before.into(), after.into()),
            (None, Some(after)) => Self::all_added(after),
            (Some(before), None) => Self::all_removed(before),
            (None, None) => Self::default(),
        }
    }
}

/// The edge delta of one relation, named as in the relation dictionaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationDelta {
    /// The relation's name.
    pub name: String,
    /// Its edge delta.
    pub edges: EdgeDelta,
}

/// The edge delta between two forward CSRs (or all-relations rows) in the
/// same id space, by merging each pair of sorted rows. O(N + M).
pub(super) fn edge_delta(before: CsrRows<'_>, after: CsrRows<'_>) -> EdgeDelta {
    let rows = before.id_space().max(after.id_space());
    let shared: u64 = (0..rows)
        .map(|source| shared_targets(before.neighbours(source), after.neighbours(source)))
        .sum();
    let (before, after) = (before.edge_count(), after.edge_count());
    EdgeDelta { before, after, added: after - shared, removed: before - shared }
}

/// The number of targets in both of two strictly ascending rows.
fn shared_targets(mut before: RowNeighbours<'_>, mut after: RowNeighbours<'_>) -> u64 {
    let mut shared = 0;
    let (mut left, mut right) = (before.next(), after.next());
    while let (Some(l), Some(r)) = (left, right) {
        match l.cmp(&r) {
            Ordering::Less => left = before.next(),
            Ordering::Greater => right = after.next(),
            Ordering::Equal => {
                shared += 1;
                left = before.next();
                right = after.next();
            }
        }
    }
    shared
}

/// The edge delta of every relation, aligned by name: `after`'s relations
/// in index order, then those found only in `before`. Each side is a
/// relation dictionary and the CSRs it indexes.
pub fn relation_deltas(
    before: (&RelationDictionary, &GraphCsr),
    after: (&RelationDictionary, &GraphCsr),
) -> Vec<RelationDelta> {
    let in_after = after.0.names().iter().filter_map(|name| {
        let edges = EdgeDelta::between(forward(before, name), Some(forward(after, name)?));
        Some(RelationDelta { name: name.clone(), edges })
    });
    let only_before = before.0.names().iter().filter_map(|name| {
        if after.0.get(name).is_some() {
            return None;
        }
        let edges = EdgeDelta::all_removed(forward(before, name)?);
        Some(RelationDelta { name: name.clone(), edges })
    });
    in_after.chain(only_before).collect()
}

/// The forward CSR of the relation called `name`.
fn forward<'g>(
    (dictionary, graph): (&RelationDictionary, &'g GraphCsr),
    name: &str,
) -> Option<&'g AnyCsr> {
    graph.relation(dictionary.get(name)?).map(RelationCsr::forward)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Csr;

    fn csr(id_space: u64, rows: &[&[u32]]) -> AnyCsr {
        let mut offsets = vec![0];
        let mut targets = Vec::new();
        for row in rows {
            targets.extend_from_slice(row);
            offsets.push(targets.len() as u64);
        }
        offsets.resize(id_space as usize + 1, targets.len() as u64);
        AnyCsr::U32(Csr::from_parts(id_space, offsets, targets, None).unwrap())
    }

    #[test]
    fn merging_rows_counts_shared_added_and_removed_edges() {
        let before = csr(3, &[&[1, 2], &[0], &[]]);
        let after = csr(4, &[&[2, 3], &[0], &[1], &[0]]);
        assert_eq!(
            edge_delta((&before).into(), (&after).into()),
            EdgeDelta { before: 3, after: 5, added: 3, removed: 1 }
        );
        assert_eq!(
            edge_delta((&after).into(), (&after).into()),
            EdgeDelta { before: 5, after: 5, added: 0, removed: 0 }
        );
    }

    #[test]
    fn a_relation_on_one_side_only_is_all_added_or_all_removed() {
        let edges = csr(2, &[&[1], &[0]]);
        assert_eq!(
            EdgeDelta::all_added(&edges),
            EdgeDelta { before: 0, after: 2, added: 2, removed: 0 }
        );
        assert_eq!(
            EdgeDelta::all_removed(&edges),
            EdgeDelta { before: 2, after: 0, added: 0, removed: 2 }
        );
    }
}
