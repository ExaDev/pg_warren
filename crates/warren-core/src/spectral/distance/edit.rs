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

use rayon::prelude::*;

use super::super::dense::BLOCK;
use super::super::{Resources, SpectralError};
use crate::graph::{AnyCsr, GraphCsr};
use crate::interrupts::{Cancelled, Interrupts};

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

/// The `edit` distance from `before` to `after`.
pub fn edit_distance<I: Interrupts>(
    resources: &mut Resources<'_, I>,
    before: &GraphCsr,
    after: &GraphCsr,
) -> Result<EditDistance, SpectralError> {
    let count = before.relations().len().max(after.relations().len());
    let mut relations = Vec::with_capacity(count);
    for relation in 0..count {
        let (b, a) = (before.relations().get(relation), after.relations().get(relation));
        relations.push(compare(resources, b.map(|r| r.forward()), a.map(|r| r.forward()))?);
    }
    let overall = relations.iter().fold(Counts::default(), |sum, r| sum + r.counts());
    Ok(EditDistance { relations, overall: overall.change() })
}

impl EdgeSetChange {
    fn counts(&self) -> Counts {
        Counts {
            added: self.added,
            removed: self.removed,
            kept: self.union - self.added - self.removed,
        }
    }
}

/// Edges in one version only, or in both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    added: u64,
    removed: u64,
    kept: u64,
}

impl std::ops::Add for Counts {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            added: self.added + other.added,
            removed: self.removed + other.removed,
            kept: self.kept + other.kept,
        }
    }
}

impl Counts {
    fn change(self) -> EdgeSetChange {
        let union = self.added + self.removed + self.kept;
        let changed = self.added + self.removed;
        let distance = if union == 0 { 0.0 } else { changed as f64 / union as f64 };
        EdgeSetChange { added: self.added, removed: self.removed, union, distance }
    }
}

/// Merges the rows of two CSRs of one relation (`None`: the version lacks
/// the relation), counting edges in either or both.
fn compare<I: Interrupts>(
    resources: &mut Resources<'_, I>,
    before: Option<&AnyCsr>,
    after: Option<&AnyCsr>,
) -> Result<EdgeSetChange, SpectralError> {
    let rows = [before, after].into_iter().flatten().map(AnyCsr::id_space).max().unwrap_or(0);
    let blocks = (rows as usize).div_ceil(BLOCK);
    let counts = resources.run(|cancellation| {
        (0..blocks)
            .into_par_iter()
            .try_fold(Counts::default, |sum, block| {
                if cancellation.is_cancelled() {
                    return Err(Cancelled.into());
                }
                let start = (block * BLOCK) as u64;
                let rows = start..rows.min(start + BLOCK as u64);
                Ok(rows.fold(sum, |sum, row| sum + merge(before, after, row)))
            })
            .try_reduce(Counts::default, |a, b| Ok(a + b))
    })?;
    Ok(counts.change())
}

/// The edges of `row` in `before` only, `after` only, and both.
fn merge(before: Option<&AnyCsr>, after: Option<&AnyCsr>, row: u64) -> Counts {
    let mut before = before.into_iter().flat_map(|csr| csr.neighbours(row)).peekable();
    let mut after = after.into_iter().flat_map(|csr| csr.neighbours(row)).peekable();
    let mut counts = Counts::default();
    loop {
        match (before.peek(), after.peek()) {
            (Some(b), Some(a)) if b == a => {
                counts.kept += 1;
                before.next();
                after.next();
            }
            (Some(b), Some(a)) if b < a => {
                counts.removed += 1;
                before.next();
            }
            (Some(_), Some(_)) | (None, Some(_)) => {
                counts.added += 1;
                after.next();
            }
            (Some(_), None) => {
                counts.removed += 1;
                before.next();
            }
            (None, None) => return counts,
        }
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests;
