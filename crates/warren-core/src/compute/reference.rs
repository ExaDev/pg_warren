//! Naive dense references for the compute kernels' property tests.

use super::{BinaryOp, Semiring, SemiringElement};
use crate::graph::{CsrLayout, DenseId, EdgeList, GraphCsr, RelationDictionary, RelationIndex};

/// A graph's edges: `(relation, source, target, weight)`.
pub(crate) type Edges = Vec<(usize, DenseId, DenseId, f64)>;

/// The relation indexes `0..count`.
pub(crate) fn relations(count: usize) -> Vec<RelationIndex> {
    let mut dictionary = RelationDictionary::new();
    (0..count).map(|r| dictionary.intern(&r.to_string()).unwrap()).collect()
}

/// The CSRs of `edges`, weighted if `weighted`.
pub(crate) fn graph(
    id_space: u64,
    relation_count: usize,
    edges: &Edges,
    weighted: bool,
    reverse: bool,
) -> GraphCsr {
    let layout = CsrLayout { build_reverse: reverse, store_merged: true };
    graph_as(id_space, relation_count, edges, weighted, layout)
}

/// [`graph`] built with `layout`. A graph of an odd number of edges is
/// read through shared views of heap segments, so that the kernels'
/// tests read both owned and shared buffers.
pub(crate) fn graph_as(
    id_space: u64,
    relation_count: usize,
    edges: &Edges,
    weighted: bool,
    layout: CsrLayout,
) -> GraphCsr {
    let indexes = relations(relation_count);
    let mut list = EdgeList::new(weighted);
    for &(relation, source, target, weight) in edges {
        list.push(indexes[relation], source, target, weight).unwrap();
    }
    let graph = GraphCsr::build_as(list, relation_count, id_space, layout).unwrap();
    match edges.len() % 2 {
        0 => graph,
        _ => graph.shared_views(),
    }
}

/// The dense adjacency matrix `A[i][j]` of the edges in `selected`
/// relations (all if `None`): the summed weight of every edge `i → j`, or
/// 1 when not `weighted`; `None` where there is no edge.
pub(crate) fn adjacency(
    id_space: u64,
    edges: &Edges,
    selected: Option<&[usize]>,
    weighted: bool,
) -> Vec<Vec<Option<f64>>> {
    let n = id_space as usize;
    let mut a = vec![vec![None; n]; n];
    for &(relation, source, target, weight) in edges {
        if selected.is_some_and(|s| !s.contains(&relation)) {
            continue;
        }
        let entry: &mut Option<f64> = &mut a[source as usize][target as usize];
        *entry = Some(match weighted {
            true => entry.unwrap_or(0.0) + weight,
            false => 1.0,
        });
    }
    a
}

/// The dense adjacency matrix of the edges in `selected` relations read
/// per relation (DESIGN.md §5.3): each relation's own entry (its edges'
/// summed weight, or 1 when not `weighted`), the relations' entries for a
/// pair folded by `fold` in relation order.
pub(crate) fn adjacency_per_relation(
    id_space: u64,
    edges: &Edges,
    selected: Option<&[usize]>,
    weighted: bool,
    fold: impl Fn(f64, f64) -> f64,
) -> Vec<Vec<Option<f64>>> {
    let mut relations: Vec<usize> = edges.iter().map(|&(relation, ..)| relation).collect();
    relations.sort_unstable();
    relations.dedup();
    relations.retain(|relation| selected.is_none_or(|selected| selected.contains(relation)));
    let n = id_space as usize;
    let mut a: Vec<Vec<Option<f64>>> = vec![vec![None; n]; n];
    for relation in relations {
        let each = adjacency(id_space, edges, Some(&[relation]), weighted);
        for (row, each) in a.iter_mut().zip(each) {
            for (entry, each) in row.iter_mut().zip(each) {
                *entry = match (*entry, each) {
                    (Some(sum), Some(each)) => Some(fold(sum, each)),
                    (entry, each) => entry.or(each),
                };
            }
        }
    }
    a
}

/// How a product's result is written to `y` (DESIGN.md §6.3).
#[derive(Debug, Clone)]
pub(crate) struct Write {
    pub(crate) selected: Vec<bool>,
    pub(crate) replace: bool,
    pub(crate) accumulate: Option<BinaryOp>,
}

/// `y ⊙= A ⊗ x` (or `Aᵀ ⊗ x`) for width-`width` rows, the naive way: for
/// each row, `⊕` over ascending columns.
pub(crate) fn product<E: SemiringElement>(
    a: &[Vec<Option<f64>>],
    transpose: bool,
    semiring: Semiring,
    width: usize,
    x: &[E],
    y: &[E],
    write: &Write,
) -> Result<Vec<E>, super::ArithmeticError> {
    let n = a.len();
    let identity = E::identity(semiring.add());
    let mut out = y.to_vec();
    for i in 0..n {
        for k in 0..width {
            let mut t: Option<E> = None;
            for j in 0..n {
                let entry = if transpose { a[j][i] } else { a[i][j] };
                let Some(weight) = entry else { continue };
                let reads_weight =
                    !matches!(semiring.multiply(), BinaryOp::Second | BinaryOp::Pair);
                let weight = if reads_weight { E::from_weight(weight)? } else { E::default() };
                let x = x[j * width + k];
                let p = E::multiply(semiring.add().op(), semiring.multiply(), weight, x)?;
                t = Some(match t {
                    None => p,
                    Some(t) => E::apply(semiring.add().op(), t, p)?,
                });
            }
            let cell = &mut out[i * width + k];
            if !write.selected[i] {
                if write.replace {
                    *cell = identity;
                }
                continue;
            }
            *cell = match (write.accumulate, t) {
                (None, t) => t.unwrap_or(identity),
                (Some(op), Some(t)) => E::apply(op, *cell, t)?,
                (Some(_), None) => *cell,
            };
        }
    }
    Ok(out)
}
