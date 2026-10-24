//! The connected components of the active nodes of an operator's matrix:
//! with more than one, eigenvalues repeat (each Laplacian has 0 once per
//! component), which single-vector Lanczos may not resolve. The deflation
//! takes the copies the components give exactly; callers warn of the rest
//! (DESIGN.md §6.5).

use super::{Resources, SpectralError};
use crate::compute::{ComputeError, MemoryCharge};
use crate::graph::{CsrRows, DenseId, GraphCsr, RelationSelection};
use crate::interrupts::Interrupts;

/// How often the pass polls the interrupts, in rows.
const ROWS_PER_POLL: u64 = 1 << 16;

/// The connected components of the active nodes, ignoring edge direction,
/// over the selected relations' forward CSRs (an edge in either direction
/// connects its ends, as in `(A + Aᵀ)/2`), numbered from 0 in the order of
/// their lowest dense ids.
#[derive(Debug)]
pub(super) struct Components {
    /// The component of each active node; 0 at inactive nodes, which are
    /// in none.
    component: Vec<DenseId>,
    count: u64,
    _charge: MemoryCharge,
}

impl Components {
    /// The components of the `active` nodes of the `relations` of `graph`.
    pub(super) fn of<I: Interrupts>(
        resources: &mut Resources<'_, I>,
        graph: &GraphCsr,
        relations: RelationSelection<'_>,
        active: &[bool],
    ) -> Result<Self, SpectralError> {
        let csrs: Vec<CsrRows<'_>> = match relations {
            RelationSelection::All => vec![graph.all_relations().forward()],
            RelationSelection::Only(indexes) => indexes
                .iter()
                .filter_map(|&relation| graph.relation(relation).map(|r| r.forward().into()))
                .collect(),
        };
        let (mut parent, charge) = resources.memory.filled(active.len(), 0 as DenseId)?;
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
        let count = number(&mut parent, active);
        Ok(Self { component: parent, count, _charge: charge })
    }

    /// The number of components.
    pub(super) fn count(&self) -> u64 {
        self.count
    }

    /// The component of each node (0 at inactive nodes).
    pub(super) fn of_nodes(&self) -> &[DenseId] {
        &self.component
    }
}

/// Replaces the union-find forest in `parent` by each active node's
/// component number, and returns the number of components. A tree's root is
/// its lowest node (a union links the higher root under the lower), so in
/// node order a root is numbered before the nodes under it.
fn number(parent: &mut [DenseId], active: &[bool]) -> u64 {
    for node in 0..parent.len() as DenseId {
        parent[node as usize] = root(parent, node);
    }
    let mut count = 0;
    for (node, &active) in active.iter().enumerate() {
        let root = parent[node] as usize;
        parent[node] = match (active, root == node) {
            (false, _) => 0,
            (true, true) => {
                count += 1;
                count - 1
            }
            (true, false) => parent[root],
        };
    }
    count
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_are_numbered_by_their_lowest_node() {
        // Trees {0, 3}, {1}, {2, 4, 5} and an inactive 6: 4 under 2, 5
        // under 4, 3 under 0.
        let mut parent = vec![0, 1, 2, 0, 2, 4, 6];
        let active = [true, true, true, true, true, true, false];
        assert_eq!(number(&mut parent, &active), 3);
        assert_eq!(parent, [0, 1, 2, 0, 2, 2, 0]);
    }
}
