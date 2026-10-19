//! The `edit` distance: the normalised symmetric difference of the edge
//! sets of two versions, per relation and over all relations.
//!
//! Edges are compared as sets of `(source, target)` dense-id pairs per
//! relation, ignoring weights (an undirected graph stores each edge in both
//! directions, which leaves the ratio unchanged). The distance is the
//! Jaccard distance `|E₁ △ E₂| / |E₁ ∪ E₂|`: 0 for equal sets, 1 for
//! disjoint ones, and growing with every edge added or removed. Relations
//! are matched by name: sibling staged versions, and other graphs, may give
//! one relation index to different relations.

use crate::graph::{GraphCsr, RelationDictionary};
use crate::version::{relation_deltas, EdgeDelta};

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

/// One relation's change, by name.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationChange {
    /// The relation's name.
    pub name: String,
    /// How its edge set changed.
    pub change: EdgeSetChange,
}

/// The `edit` distance between two versions.
#[derive(Debug, Clone, PartialEq)]
pub struct EditDistance {
    /// Each relation's change, matched by name: the later version's
    /// relations in index order, then those only in the earlier.
    pub relations: Vec<RelationChange>,
    /// The change of the set of `(relation, source, target)` edges.
    pub overall: EdgeSetChange,
}

/// The `edit` distance from `before` to `after`, each a relation
/// dictionary and its CSRs, by merging the sorted rows of the forward CSRs
/// of each relation of either version as `warren.diff` does (O(N + M)).
pub fn edit_distance(
    before: (&RelationDictionary, &GraphCsr),
    after: (&RelationDictionary, &GraphCsr),
) -> EditDistance {
    let deltas = relation_deltas(before, after);
    let overall = deltas.iter().fold(EdgeDelta::default(), |sum, delta| EdgeDelta {
        before: sum.before + delta.edges.before,
        after: sum.after + delta.edges.after,
        added: sum.added + delta.edges.added,
        removed: sum.removed + delta.edges.removed,
    });
    let relations = deltas
        .into_iter()
        .map(|delta| RelationChange { name: delta.name, change: delta.edges.into() })
        .collect();
    EditDistance { relations, overall: overall.into() }
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
