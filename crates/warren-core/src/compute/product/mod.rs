//! Semiring products of the graph's adjacency matrix with a vector (SpMV)
//! or a width-d matrix (SpMM), with GraphBLAS masks, `replace` and
//! accumulation (DESIGN.md §6.3).
//!
//! `A[i][j]` is the weight of the edge `i → j` (1 when unweighted), summed
//! over the selected relations. `transpose = false` computes `y = A ⊗ x`,
//! `y_i = ⊕_{i → j} ⊗(A[i][j], x_j)`, row-parallel over the forward CSR.
//! `transpose = true` computes `y = Aᵀ ⊗ x`, `y_i = ⊕_{j → i} ⊗(A[j][i],
//! x_j)`: the gather from in-neighbours, row-parallel over the reverse CSR,
//! or scattered (pushed) over the forward CSR when the graph has no reverse
//! CSR. Either way `⊕` folds a row's entries in ascending dense-id order, so
//! the two forms agree bit for bit.

mod kernel;
mod matrix;

use kernel::{PushScratch, Write};
use matrix::{Form, Matrix};

use super::{
    AnyVector, BinaryOp, ComputeError, ComputeMemory, Semiring, SemiringElement, Vector, Workers,
};
use crate::graph::GraphCsr;
use crate::traversal::{Interrupts, RelationSelection};

/// A value mask (GraphBLAS semantics): row `i` is selected when the mask's
/// value there is non-zero (`NaN` included), or, with `complement`, when it
/// is zero.
#[derive(Debug, Clone, Copy)]
pub struct Mask<'a> {
    /// A width-1 vector of any element type.
    pub vector: &'a AnyVector,
    /// Select the rows whose mask value is zero instead.
    pub complement: bool,
}

/// What to multiply and how to write the result to `y`.
#[derive(Debug, Clone, Copy)]
pub struct Product<'a> {
    /// The semiring `⊕.⊗`.
    pub semiring: Semiring,
    /// Multiply by `Aᵀ` (gather from in-neighbours) instead of `A`.
    pub transpose: bool,
    /// The relations whose edges make up `A`.
    pub relations: RelationSelection<'a>,
    /// Use edge weights (`None`: if the graph has them); without them every
    /// weight is 1.
    pub weighted: Option<bool>,
    /// Only rows the mask selects are written.
    pub mask: Option<Mask<'a>>,
    /// Set rows the mask does not select to the `⊕` identity instead of
    /// keeping them.
    pub replace: bool,
    /// `y_i = accumulate(y_i, t_i)` for rows with a result `t_i`, keeping
    /// `y_i` where the row of `A` is empty; `None` overwrites `y_i`, with
    /// the `⊕` identity where the row is empty.
    pub accumulate: Option<BinaryOp>,
}

impl Product<'_> {
    /// `y = A ⊗ x` over `semiring`: every relation, weights if present, no
    /// mask, overwriting `y`.
    pub fn new(semiring: Semiring) -> Self {
        Self {
            semiring,
            transpose: false,
            relations: RelationSelection::All,
            weighted: None,
            mask: None,
            replace: false,
            accumulate: None,
        }
    }
}

/// SpMV: `y = A ⊗ x` (or `Aᵀ ⊗ x`) for width-1 vectors `x` and `y` of the
/// same element type.
pub fn spmv<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &GraphCsr,
    product: &Product<'_>,
    x: &AnyVector,
    y: &mut AnyVector,
) -> Result<(), ComputeError> {
    for width in [x.width(), y.width()] {
        if width != 1 {
            return Err(ComputeError::WidthMismatch { expected: 1, found: width });
        }
    }
    multiply(workers, memory, interrupts, graph, product, x, y)
}

/// SpMM: `Y = A ⊗ X` (or `Aᵀ ⊗ X`) for matrices `X` and `Y` of the same
/// width and element type: row `i` of `Y` combines the rows of `X` at
/// `i`'s neighbours, elementwise.
pub fn spmm<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &GraphCsr,
    product: &Product<'_>,
    x: &AnyVector,
    y: &mut AnyVector,
) -> Result<(), ComputeError> {
    if x.width() != y.width() {
        return Err(ComputeError::WidthMismatch { expected: y.width(), found: x.width() });
    }
    multiply(workers, memory, interrupts, graph, product, x, y)
}

/// Checks that the operands fit the graph and each other, then dispatches
/// on the element type.
fn multiply<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &GraphCsr,
    product: &Product<'_>,
    x: &AnyVector,
    y: &mut AnyVector,
) -> Result<(), ComputeError> {
    let id_space = graph.all_relations().forward().id_space();
    if x.dtype() != y.dtype() {
        return Err(ComputeError::DtypeMismatch { expected: y.dtype(), found: x.dtype() });
    }
    let mask = product.mask.map(|mask| mask.vector);
    for vector in [Some(x), Some(&*y), mask].into_iter().flatten() {
        if vector.id_space() != id_space {
            return Err(ComputeError::IdSpaceMismatch {
                expected: id_space,
                found: vector.id_space(),
            });
        }
    }
    if let Some(mask) = mask.filter(|mask| mask.width() != 1) {
        return Err(ComputeError::WidthMismatch { expected: 1, found: mask.width() });
    }
    let matrix = Matrix::new(graph, product.relations, product.transpose, product.weighted)?;
    let mut run = Run { workers, memory, interrupts, matrix: &matrix, product };
    match y {
        AnyVector::F32(y) => run.typed(x, y),
        AnyVector::F64(y) => run.typed(x, y),
        AnyVector::I64(y) => run.typed(x, y),
        AnyVector::Bool(y) => run.typed(x, y),
    }
}

/// A checked product, ready to run at an element type.
struct Run<'r, 'g, I> {
    workers: &'r Workers,
    memory: &'r ComputeMemory,
    interrupts: &'r mut I,
    matrix: &'r Matrix<'g>,
    product: &'r Product<'r>,
}

impl<I: Interrupts> Run<'_, '_, I> {
    fn typed<E: SemiringElement>(
        &mut self,
        x: &AnyVector,
        y: &mut Vector<E>,
    ) -> Result<(), ComputeError> {
        let x = E::vector(x)
            .ok_or(ComputeError::DtypeMismatch { expected: E::DTYPE, found: x.dtype() })?;
        let (matrix, semiring) = (self.matrix, self.product.semiring);
        let write = Write::new(self.product);
        let y = y.values_mut();
        match matrix.form() {
            Form::Pull => self.workers.run(self.interrupts, |cancellation| {
                kernel::pull(semiring, cancellation, matrix, x, y, &write)
            }),
            Form::Push => {
                let (mut sums, _sums) = self.memory.filled(y.len(), E::default())?;
                let (mut present, _present) = self.memory.filled(x.id_space() as usize, false)?;
                let scratch = PushScratch { sums: &mut sums, present: &mut present };
                self.workers.run(self.interrupts, |cancellation| {
                    kernel::push(semiring, cancellation, matrix, x, y, scratch, &write)
                })
            }
        }
    }
}

#[cfg(test)]
mod tests;
