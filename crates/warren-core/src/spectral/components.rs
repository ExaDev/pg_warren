//! The connected components of the active nodes of an operator's matrix:
//! with more than one, eigenvalues repeat (each Laplacian has 0 once per
//! component), which single-vector Lanczos may not resolve, so callers
//! warn (DESIGN.md §6.5).

use super::{Resources, SpectralError};
use crate::compute::ComputeError;
use crate::graph::{DenseId, GraphCsr, MergedRows, RelationSelection};
use crate::interrupts::Interrupts;

/// How often the pass polls the interrupts, in rows.
const ROWS_PER_POLL: u64 = 1 << 16;

/// The number of connected components among the `active` nodes, ignoring
/// edge direction, over the selected relations' forward CSRs (an edge in
/// either direction connects its ends, as in `(A + Aᵀ)/2`).
pub(super) fn components<I: Interrupts>(
    resources: &mut Resources<'_, I>,
    graph: &GraphCsr,
    relations: RelationSelection<'_>,
    active: &[bool],
) -> Result<u64, SpectralError> {
    let csrs: Vec<MergedRows<'_>> = match relations {
        RelationSelection::All => vec![graph.all_relations().forward()],
        RelationSelection::Only(indexes) => indexes
            .iter()
            .filter_map(|&relation| graph.relation(relation).map(|r| r.forward().into()))
            .collect(),
    };
    let (mut parent, _charge) = resources.memory.filled(active.len(), 0 as DenseId)?;
    for (node, parent) in (0..).zip(parent.iter_mut()) {
        *parent = node;
    }
    for csr in csrs {
        for source in 0..csr.id_space() {
            if source.is_multiple_of(ROWS_PER_POLL) {
                resources.interrupts.check().map_err(ComputeError::from)?;
            }
            for target in csr.neighbours(source) {
                let (a, b) = (root(&mut parent, source), root(&mut parent, target));
                parent[a.max(b) as usize] = a.min(b);
            }
        }
    }
    let roots =
        (0..).zip(active).filter(|&(node, &active)| active && root(&mut parent, node) == node);
    Ok(roots.count() as u64)
}

/// The root of `node`'s tree, halving the path to it on the way.
fn root(parent: &mut [DenseId], mut node: DenseId) -> DenseId {
    while parent[node as usize] != node {
        let grandparent = parent[parent[node as usize] as usize];
        parent[node as usize] = grandparent;
        node = grandparent;
    }
    node
}
