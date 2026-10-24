//! The connected components of the active nodes of an operator's matrix,
//! and which of them are bipartite: with more than one component,
//! eigenvalues repeat (each Laplacian has 0 once per component, the random
//! walk −1 once per bipartite one), which single-vector Lanczos may not
//! resolve. The deflation takes the copies the components give exactly;
//! callers warn of the rest (DESIGN.md §6.5).

use super::{Resources, SpectralError};
use crate::compute::{ComputeError, MemoryCharge};
use crate::graph::{CsrRows, DenseId, GraphCsr, RelationSelection};
use crate::interrupts::Interrupts;

/// How often the pass polls the interrupts, in rows.
const ROWS_PER_POLL: u64 = 1 << 16;

/// The connected components of the active nodes, ignoring edge direction,
/// over the selected relations' forward CSRs (an edge in either direction
/// connects its ends, as in `(A + Aᵀ)/2`), numbered from 0 in the order of
/// their lowest dense ids. Only edges between active nodes connect: an
/// unsymmetrised matrix's inactive node (one without a forward row) is in
/// no component, whatever links into it. Edges connect by structure, so
/// one of weight 0 still does: each component's vectors are then still
/// eigenvectors, of fewer components than the matrix has.
#[derive(Debug)]
pub(super) struct Components {
    /// The component of each active node; 0 at inactive nodes, which are
    /// in none.
    component: Vec<DenseId>,
    /// Each active node's colour in a two-colouring of its component, the
    /// lowest node's `false`; meaningful only in bipartite components.
    colour: Vec<bool>,
    /// Whether each component is bipartite (has no odd cycle, a self-loop
    /// being one).
    bipartite: Vec<bool>,
    _charges: [MemoryCharge; 3],
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
        let n = active.len();
        let (parent, parent_charge) = resources.memory.filled(n, 0 as DenseId)?;
        let (parity, parity_charge) = resources.memory.filled(n, false)?;
        let (odd, _odd_charge) = resources.memory.filled(n, false)?;
        let mut forest = Forest { parent, parity, odd };
        for (node, parent) in (0..).zip(forest.parent.iter_mut()) {
            *parent = node;
        }
        for csr in csrs {
            for source in 0..csr.id_space() {
                if source.is_multiple_of(ROWS_PER_POLL) {
                    resources.interrupts.check().map_err(ComputeError::from)?;
                }
                if !active[source as usize] {
                    continue;
                }
                for target in csr.neighbours(source) {
                    if active[target as usize] {
                        forest.link(source, target);
                    }
                }
            }
        }
        let count = forest.compress(active);
        let (mut bipartite, bipartite_charge) = resources.memory.filled(count, false)?;
        let Forest { parent: mut component, parity: colour, odd } = forest;
        number(&mut component, active, &odd, &mut bipartite);
        Ok(Self {
            component,
            colour,
            bipartite,
            _charges: [parent_charge, parity_charge, bipartite_charge],
        })
    }

    /// The number of components.
    pub(super) fn count(&self) -> u64 {
        self.bipartite.len() as u64
    }

    /// The component of each node (0 at inactive nodes).
    pub(super) fn of_nodes(&self) -> &[DenseId] {
        &self.component
    }

    /// Each node's colour in its component's two-colouring.
    pub(super) fn colours(&self) -> &[bool] {
        &self.colour
    }

    /// Whether each component is bipartite.
    pub(super) fn bipartite(&self) -> &[bool] {
        &self.bipartite
    }
}

/// A union-find forest whose links record parity: `parity[x]` is whether
/// `x` and `parent[x]` take different colours, and `odd[r]` whether the
/// tree rooted at `r` has an odd cycle.
struct Forest {
    parent: Vec<DenseId>,
    parity: Vec<bool>,
    odd: Vec<bool>,
}

impl Forest {
    /// Joins the trees of an edge's ends, the higher root under the lower
    /// so that a tree's root is its lowest node, with the ends coloured
    /// apart; within one tree, an edge between equal colours closes an odd
    /// cycle.
    fn link(&mut self, source: DenseId, target: DenseId) {
        let ((a, colour_a), (b, colour_b)) = (self.find(source), self.find(target));
        if a == b {
            self.odd[a as usize] |= colour_a == colour_b;
            return;
        }
        let (low, high) = (a.min(b), a.max(b));
        self.parent[high as usize] = low;
        self.parity[high as usize] = colour_a == colour_b;
        self.odd[low as usize] |= self.odd[high as usize];
    }

    /// The root of `node`'s tree and `node`'s colour relative to it,
    /// halving the path to it on the way.
    fn find(&mut self, mut node: DenseId) -> (DenseId, bool) {
        let mut colour = false;
        while self.parent[node as usize] != node {
            let up = self.parent[node as usize];
            self.parity[node as usize] ^= self.parity[up as usize];
            self.parent[node as usize] = self.parent[up as usize];
            colour ^= self.parity[node as usize];
            node = self.parent[node as usize];
        }
        (node, colour)
    }

    /// Points every node at its root, leaving its colour in `parity`, and
    /// returns the number of the active nodes' trees.
    fn compress(&mut self, active: &[bool]) -> usize {
        let mut count = 0;
        for node in 0..self.parent.len() as DenseId {
            let (root, colour) = self.find(node);
            self.parent[node as usize] = root;
            self.parity[node as usize] = colour;
            count += usize::from(root == node && active[node as usize]);
        }
        count
    }
}

/// Replaces each node's root in `parent` by its component's number, and
/// records in `bipartite` whether each component's root has no odd cycle.
/// Only active nodes are linked, so a tree's root, its lowest node, is
/// active wherever the tree is, and in node order is numbered before the
/// nodes under it.
fn number(parent: &mut [DenseId], active: &[bool], odd: &[bool], bipartite: &mut [bool]) {
    let mut count = 0;
    for (node, &active) in active.iter().enumerate() {
        let root = parent[node] as usize;
        parent[node] = match (active, root == node) {
            (false, _) => 0,
            (true, true) => {
                bipartite[count as usize] = !odd[node];
                count += 1;
                count - 1
            }
            (true, false) => parent[root],
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_are_numbered_by_their_lowest_node() {
        // Roots {0, 3}, {1}, {2, 4, 5} and an inactive 6; {2, 4, 5} has an
        // odd cycle.
        let mut parent = vec![0, 1, 2, 0, 2, 2, 6];
        let active = [true, true, true, true, true, true, false];
        let odd = [false, false, true, false, false, false, false];
        let mut bipartite = [false; 3];
        number(&mut parent, &active, &odd, &mut bipartite);
        assert_eq!(parent, [0, 1, 2, 0, 2, 2, 0]);
        assert_eq!(bipartite, [true, true, false]);
    }

    fn forest(n: usize, edges: &[(DenseId, DenseId)]) -> (Forest, usize) {
        let mut forest = Forest {
            parent: (0..n as DenseId).collect(),
            parity: vec![false; n],
            odd: vec![false; n],
        };
        for &(source, target) in edges {
            forest.link(source, target);
        }
        let count = forest.compress(&vec![true; n]);
        (forest, count)
    }

    #[test]
    fn a_tree_or_an_even_cycle_is_two_coloured_and_an_odd_cycle_is_not() {
        // A path 4 – 2 – 0 – 3, a 4-cycle 1 – 5 – 6 – 7 – 1 and a triangle
        // 8 – 9 – 10, linked in an order that joins trees below their roots.
        let edges =
            [(4, 2), (3, 0), (2, 0), (5, 6), (7, 1), (6, 7), (1, 5), (9, 10), (10, 8), (8, 9)];
        let (forest, count) = forest(11, &edges);
        assert_eq!(count, 3);
        assert_eq!(forest.parent, [0, 1, 0, 0, 0, 1, 1, 1, 8, 8, 8]);
        let colours = &forest.parity;
        for &(source, target) in &edges[..7] {
            assert_ne!(colours[source as usize], colours[target as usize], "{source} – {target}");
        }
        assert!(!colours[0] && !colours[1]);
        assert_eq!([forest.odd[0], forest.odd[1], forest.odd[8]], [false, false, true]);
    }

    #[test]
    fn a_self_loop_is_an_odd_cycle() {
        let (forest, _) = forest(2, &[(0, 1), (1, 1)]);
        assert!(forest.odd[0]);
    }
}
