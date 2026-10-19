//! The product kernels: pull (row-parallel, no write conflicts) and push
//! (a sequential scatter, then a row-parallel write), and how a row's
//! result is written to `y` under the mask.

use rayon::prelude::*;

use super::matrix::Matrix;
use super::{Mask, Product};
use crate::compute::{
    AnyVector, ArithmeticError, BinaryOp, ComputeError, Element, Semiring, SemiringElement, Vector,
};
use crate::graph::DenseId;
use crate::traversal::{Cancellation, Cancelled};

/// The rows one task computes; a task checks the cancellation flag first.
/// Small under test, so tests cross task boundaries.
const ROWS_PER_TASK: usize = if cfg!(test) { 3 } else { 4096 };

/// How often the push scatter checks the cancellation flag, in rows.
const ROWS_PER_CHECK: u64 = 1 << 16;

/// `y = A ⊗ x` row by row, in parallel.
pub(super) fn pull<E: SemiringElement>(
    cancellation: &Cancellation,
    matrix: &Matrix<'_>,
    semiring: Semiring,
    x: &Vector<E>,
    y: &mut [E],
    write: &Write<'_, E>,
) -> Result<(), ComputeError> {
    let width = x.width();
    y.par_chunks_mut(ROWS_PER_TASK * width).enumerate().try_for_each(|(task, rows)| {
        if cancellation.is_cancelled() {
            return Err(Cancelled.into());
        }
        let mut sum = vec![E::default(); width];
        let mut scratch = Vec::new();
        for (offset, out) in rows.chunks_mut(width).enumerate() {
            let row = (task * ROWS_PER_TASK + offset) as DenseId;
            let mut present = false;
            matrix.for_each_entry(row, &mut scratch, |column, weight| {
                fold(semiring, &mut sum, present, weight, x.row(column))?;
                present = true;
                Ok(())
            })?;
            write.row(row, out, present.then_some(&sum))?;
        }
        Ok(())
    })
}

/// The scratch a push needs: a sum row per node and whether it has an
/// entry yet.
pub(super) struct PushScratch<'s, E> {
    pub(super) sums: &'s mut [E],
    pub(super) present: &'s mut [bool],
}

/// `y = Aᵀ ⊗ x` by scattering each row of `A` into the sums of its
/// columns, in ascending row order (so each sum folds its entries in the
/// order a pull over `Aᵀ` would), then writing every row in parallel.
pub(super) fn push<E: SemiringElement>(
    cancellation: &Cancellation,
    matrix: &Matrix<'_>,
    semiring: Semiring,
    x: &Vector<E>,
    y: &mut [E],
    scratch: PushScratch<'_, E>,
    write: &Write<'_, E>,
) -> Result<(), ComputeError> {
    let width = x.width();
    let PushScratch { sums, present } = scratch;
    let mut entries = Vec::new();
    for row in 0..x.id_space() {
        if row % ROWS_PER_CHECK == 0 && cancellation.is_cancelled() {
            return Err(Cancelled.into());
        }
        let values = x.row(row);
        matrix.for_each_entry(row, &mut entries, |column, weight| {
            let column = column as usize;
            let sum = &mut sums[column * width..(column + 1) * width];
            fold(semiring, sum, present[column], weight, values)?;
            present[column] = true;
            Ok(())
        })?;
    }
    let task = ROWS_PER_TASK * width;
    y.par_chunks_mut(task).zip(sums.par_chunks(task)).enumerate().try_for_each(
        |(index, (rows, sums))| {
            if cancellation.is_cancelled() {
                return Err(Cancelled.into());
            }
            for (offset, (out, sum)) in rows.chunks_mut(width).zip(sums.chunks(width)).enumerate() {
                let row = index * ROWS_PER_TASK + offset;
                write.row(row as DenseId, out, present[row].then_some(sum))?;
            }
            Ok(())
        },
    )
}

/// `sum ⊕= ⊗(weight, values)` elementwise, or `sum = ⊗(weight, values)`
/// for a row's first entry.
fn fold<E: SemiringElement>(
    semiring: Semiring,
    sum: &mut [E],
    present: bool,
    weight: f64,
    values: &[E],
) -> Result<(), ArithmeticError> {
    let multiply = semiring.multiply();
    let weight = match multiply {
        BinaryOp::Second | BinaryOp::Pair => E::default(),
        _ => E::from_weight(weight)?,
    };
    let add = semiring.add().op();
    for (sum, &value) in sum.iter_mut().zip(values) {
        let product = E::apply(multiply, weight, value)?;
        *sum = if present { E::apply(add, *sum, product)? } else { product };
    }
    Ok(())
}

/// How a row's result `t` is written to `y`.
#[derive(Debug)]
pub(super) struct Write<'a, E> {
    selection: Option<Selection<'a>>,
    replace: bool,
    accumulate: Option<BinaryOp>,
    identity: E,
}

impl<'a, E: SemiringElement> Write<'a, E> {
    pub(super) fn new(product: &Product<'a>) -> Self {
        Self {
            selection: product.mask.map(Selection::new),
            replace: product.replace,
            accumulate: product.accumulate,
            identity: E::identity(product.semiring.add()),
        }
    }

    /// Writes `t` (`None`: the row of `A` is empty) to `out`, row `row` of
    /// `y`.
    fn row(&self, row: DenseId, out: &mut [E], t: Option<&[E]>) -> Result<(), ArithmeticError> {
        if !self.selection.as_ref().is_none_or(|selection| selection.selects(row)) {
            if self.replace {
                out.fill(self.identity);
            }
            return Ok(());
        }
        match (self.accumulate, t) {
            (None, Some(t)) => out.copy_from_slice(t),
            (None, None) => out.fill(self.identity),
            (Some(op), Some(t)) => {
                for (out, &t) in out.iter_mut().zip(t) {
                    *out = E::apply(op, *out, t)?;
                }
            }
            (Some(_), None) => {}
        }
        Ok(())
    }
}

/// The rows a mask selects.
#[derive(Debug)]
struct Selection<'a> {
    mask: &'a AnyVector,
    complement: bool,
}

impl<'a> Selection<'a> {
    fn new(mask: Mask<'a>) -> Self {
        Self { mask: mask.vector, complement: mask.complement }
    }

    fn selects(&self, row: DenseId) -> bool {
        let row = row as usize;
        let set = match self.mask {
            AnyVector::F32(mask) => mask.values()[row].is_nonzero(),
            AnyVector::F64(mask) => mask.values()[row].is_nonzero(),
            AnyVector::I64(mask) => mask.values()[row].is_nonzero(),
            AnyVector::Bool(mask) => mask.values()[row].is_nonzero(),
        };
        set != self.complement
    }
}
