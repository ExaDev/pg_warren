//! The `edit` distance to a version of another graph, through an
//! [`IdAlignment`], without rebuilding its CSRs: each of its edges is
//! mapped into the shared dense ids and looked up, by binary search, in
//! this version's row of the same relation (matched by name). It holds
//! nothing beside the two versions and the alignment, so it charges
//! nothing to the compute budget.

use super::{summarised, EditDistance};
use crate::compute::ComputeError;
use crate::graph::{AnyCsr, DenseId, GraphCsr, IdKeys, RelationCsr, RelationDictionary, Target};
use crate::interrupts::Interrupts;
use crate::spectral::distance::graphs::ROWS_PER_POLL;
use crate::spectral::{IdAlignment, SpectralError};
use crate::version::{EdgeDelta, GraphVersion, RelationDelta};

/// The `edit` distance from `this` to `other`, a version of another graph
/// that `alignment` (made by `align_ids` for `other`) maps into `this`'s id
/// space: as [`super::edit_distance`] of `this` and `other`'s CSRs rebuilt
/// in the shared ids, relations in the order of `alignment`'s dictionary.
/// O(M log d) for `other`'s M edges and `this`'s rows of at most d,
/// polling the interrupts every 64K rows.
pub fn aligned_edit_distance<K: IdKeys, I: Interrupts>(
    interrupts: &mut I,
    this: &GraphVersion<'_, K>,
    other: &GraphVersion<'_, K>,
    alignment: &IdAlignment,
) -> Result<EditDistance, SpectralError> {
    let dense_ids = alignment.dense_ids.as_slice();
    if dense_ids.len() as u64 != other.id_map().id_space() {
        let (expected, found) = (other.id_map().id_space(), dense_ids.len() as u64);
        return Err(SpectralError::MappingLength { expected, found });
    }
    let mut deltas = Vec::new();
    for name in alignment.relations.names() {
        let before = forward((this.relations(), this.graph()), name);
        let after = forward((other.relations(), other.graph()), name);
        let shared = match (before, after) {
            (Some(before), Some(after)) => shared_edges(interrupts, before, after, dense_ids)?,
            _ => 0,
        };
        let (before, after) = (edge_count(before), edge_count(after));
        let edges = EdgeDelta { before, after, added: after - shared, removed: before - shared };
        deltas.push(RelationDelta { name: name.clone(), edges });
    }
    Ok(summarised(deltas))
}

/// The forward CSR of the relation called `name`.
fn forward<'g>(
    (dictionary, graph): (&RelationDictionary, &'g GraphCsr),
    name: &str,
) -> Option<&'g AnyCsr> {
    graph.relation(dictionary.get(name)?).map(RelationCsr::forward)
}

fn edge_count(csr: Option<&AnyCsr>) -> u64 {
    csr.map_or(0, AnyCsr::edge_count)
}

/// The edges of `other` whose ends, mapped by `dense_ids`, are an edge of
/// `this`.
fn shared_edges<I: Interrupts>(
    interrupts: &mut I,
    this: &AnyCsr,
    other: &AnyCsr,
    dense_ids: &[DenseId],
) -> Result<u64, SpectralError> {
    let mut shared = 0;
    for source in 0..other.id_space() {
        if source.is_multiple_of(ROWS_PER_POLL) {
            interrupts.check().map_err(ComputeError::from)?;
        }
        let targets = other.neighbours(source);
        let mapped = dense_ids[source as usize];
        shared += match this {
            AnyCsr::U32(csr) => shared_targets(csr.neighbours(mapped), targets, dense_ids),
            AnyCsr::U64(csr) => shared_targets(csr.neighbours(mapped), targets, dense_ids),
        };
    }
    Ok(shared)
}

/// How many of `targets`, mapped by `dense_ids`, are in `row`, which is
/// strictly ascending.
fn shared_targets<T: Target>(
    row: &[T],
    targets: impl Iterator<Item = DenseId>,
    dense_ids: &[DenseId],
) -> u64 {
    if row.is_empty() {
        return 0;
    }
    let in_row = |target: DenseId| {
        T::from_dense(dense_ids[target as usize]).is_some_and(|t| row.binary_search(&t).is_ok())
    };
    targets.filter(|&target| in_row(target)).count() as u64
}

// Not under Miri: these tests build the compute pool, whose work stealing
// runs through crossbeam-epoch, which Miri's Stacked Borrows model rejects
// (see compute::workers).
#[cfg(test)]
#[cfg(not(miri))]
mod tests;
