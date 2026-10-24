//! Dense references for the spectral tests: each operator as a dense
//! matrix, and its eigenpairs from nalgebra's dense symmetric eigensolver.

use nalgebra::{DMatrix, DVector, SymmetricEigen};

use super::Operator;
use crate::compute::reference::{self, Edges};

/// A dense operator and which nodes it is restricted to.
#[derive(Debug, Clone)]
pub(crate) struct Dense {
    /// The operator over the whole id space; rows and columns of inactive
    /// nodes are zero.
    pub(crate) matrix: DMatrix<f64>,
    /// Whether each node has an edge (after symmetrisation).
    pub(crate) active: Vec<bool>,
}

/// The dense matrix of `operator` over `edges`.
pub(crate) fn operator(
    id_space: u64,
    edges: &Edges,
    selected: Option<&[usize]>,
    weighted: bool,
    symmetrize: bool,
    operator: Operator,
) -> Dense {
    let n = id_space as usize;
    let a = reference::adjacency(id_space, edges, selected, weighted);
    let a = DMatrix::from_fn(n, n, |i, j| a[i][j].unwrap_or(0.0));
    dense(id_space, edges, selected, a, symmetrize, operator)
}

/// The dense matrix of `operator` over `edges` read per relation (DESIGN.md
/// §5.3): `A` is the sum of each selected relation's own adjacency matrix,
/// so an unweighted pair counts once in each relation linking it.
pub(crate) fn per_relation_operator(
    id_space: u64,
    edges: &Edges,
    selected: Option<&[usize]>,
    weighted: bool,
    symmetrize: bool,
    operator: Operator,
) -> Dense {
    let n = id_space as usize;
    let mut relations: Vec<usize> = edges.iter().map(|&(relation, ..)| relation).collect();
    relations.sort_unstable();
    relations.dedup();
    relations.retain(|relation| selected.is_none_or(|selected| selected.contains(relation)));
    let a = relations.iter().fold(DMatrix::zeros(n, n), |sum, &relation| {
        let a = reference::adjacency(id_space, edges, Some(&[relation]), weighted);
        sum + DMatrix::from_fn(n, n, |i, j| a[i][j].unwrap_or(0.0))
    });
    dense(id_space, edges, selected, a, symmetrize, operator)
}

/// The dense matrix of `operator` for adjacency matrix `a` of `edges`.
fn dense(
    id_space: u64,
    edges: &Edges,
    selected: Option<&[usize]>,
    a: DMatrix<f64>,
    symmetrize: bool,
    operator: Operator,
) -> Dense {
    let n = id_space as usize;
    let present = |i: usize, j: usize| {
        edges.iter().any(|&(relation, s, t, _)| {
            selected.is_none_or(|selected| selected.contains(&relation))
                && ((s as usize, t as usize) == (i, j)
                    || (symmetrize && (t as usize, s as usize) == (i, j)))
        })
    };
    let active: Vec<bool> = (0..n).map(|i| (0..n).any(|j| present(i, j))).collect();
    let s = if symmetrize { (&a + a.transpose()) / 2.0 } else { a };
    let degrees: Vec<f64> = (0..n).map(|i| s.row(i).sum()).collect();
    let scale: Vec<f64> =
        (0..n).map(|i| if active[i] { 1.0 / degrees[i].sqrt() } else { 0.0 }).collect();
    let identity = DMatrix::from_fn(n, n, |i, j| f64::from(u8::from(i == j && active[i])));
    let normalized = DMatrix::from_fn(n, n, |i, j| scale[i] * s[(i, j)] * scale[j]);
    let matrix = match operator {
        Operator::Adjacency => s,
        Operator::Laplacian => DMatrix::from_diagonal(&DVector::from_vec(degrees)) - s,
        Operator::NormalizedLaplacian => identity - normalized,
        Operator::RandomWalk => normalized,
    };
    Dense { matrix, active }
}

impl Dense {
    /// The eigenvalues of the operator restricted to the active nodes,
    /// ascending, with unit eigenvectors over the whole id space (zero at
    /// inactive nodes).
    pub(crate) fn eigenpairs(&self) -> Vec<(f64, DVector<f64>)> {
        let nodes: Vec<usize> = (0..self.active.len()).filter(|&i| self.active[i]).collect();
        let m = nodes.len();
        let restricted = DMatrix::from_fn(m, m, |i, j| self.matrix[(nodes[i], nodes[j])]);
        let eigen = SymmetricEigen::new(restricted);
        let mut pairs: Vec<(f64, DVector<f64>)> = (0..m)
            .map(|p| {
                let mut vector = DVector::zeros(self.active.len());
                for (i, &node) in nodes.iter().enumerate() {
                    vector[node] = eigen.eigenvectors[(i, p)];
                }
                (eigen.eigenvalues[p], vector)
            })
            .collect();
        pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
        pairs
    }
}
