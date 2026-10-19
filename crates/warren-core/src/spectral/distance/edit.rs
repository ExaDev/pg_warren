//! The `edit` distance: the normalised symmetric difference of the edge
//! sets of two versions, per relation and over all relations.
//!
//! Edges are compared as sets of `(source, target)` dense-id pairs per
//! relation, ignoring weights (an undirected graph stores each edge in both
//! directions, which leaves the ratio unchanged). The distance is the
//! Jaccard distance `|E₁ △ E₂| / |E₁ ∪ E₂|`: 0 for equal sets, 1 for
//! disjoint ones, and growing with every edge added or removed. Relations
//! are matched by relation index, which the append-only relation
//! dictionary keeps stable across versions (DESIGN.md §5.3).

use crate::graph::{AnyCsr, GraphCsr};
use crate::version::EdgeDelta;

/// How one edge set changed between two versions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeSetChange {
    /// Edges only in the later version.
    pub added: u64,
    /// Edges only in the earlier version.
    pub removed: u64,
    /// Edges in either version.
    pub union: u64,
    /// `(added + removed) / union`, 0 when both sets are empty.
    pub distance: f64,
}

/// The `edit` distance between two versions.
#[derive(Debug, Clone, PartialEq)]
pub struct EditDistance {
    /// Each relation's change, in relation-index order, over the relations
    /// of either version.
    pub relations: Vec<EdgeSetChange>,
    /// The change of the set of `(relation, source, target)` edges.
    pub overall: EdgeSetChange,
}

/// The `edit` distance from `before` to `after`, by merging the sorted
/// rows of their forward CSRs as `warren.diff` does (O(N + M)).
pub fn edit_distance(before: &GraphCsr, after: &GraphCsr) -> EditDistance {
    let count = before.relations().len().max(after.relations().len());
    let deltas: Vec<EdgeDelta> =
        (0..count).map(|r| EdgeDelta::between(forward(before, r), forward(after, r))).collect();
    let overall = deltas.iter().fold(EdgeDelta::default(), |sum, delta| EdgeDelta {
        before: sum.before + delta.before,
        after: sum.after + delta.after,
        added: sum.added + delta.added,
        removed: sum.removed + delta.removed,
    });
    EditDistance {
        relations: deltas.into_iter().map(EdgeSetChange::from).collect(),
        overall: overall.into(),
    }
}

/// The forward CSR of relation index `relation`, if the version has it.
fn forward(graph: &GraphCsr, relation: usize) -> Option<&AnyCsr> {
    graph.relations().get(relation).map(|r| r.forward())
}

impl From<EdgeDelta> for EdgeSetChange {
    fn from(delta: EdgeDelta) -> Self {
        let union = delta.before + delta.added;
        let changed = delta.added + delta.removed;
        let distance = if union == 0 { 0.0 } else { changed as f64 / union as f64 };
        Self { added: delta.added, removed: delta.removed, union, distance }
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests;
