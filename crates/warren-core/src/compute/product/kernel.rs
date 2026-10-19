//! The product kernels: pull (row-parallel, no write conflicts) and push
//! (a sequential scatter, then a row-parallel write), and how a row's
//! result is written to `y` under the mask.

use rayon::prelude::*;

use super::matrix::Matrix;
use super::{Mask, Product};
use crate::compute::{ArithmeticError, BinaryOp, ComputeError, Semiring, SemiringElement, Vector};
use crate::graph::DenseId;
use crate::interrupts::{Cancellation, Cancelled};

/// The rows one task computes; a task checks the cancellation flag first.
/// Small under test, so tests cross task boundaries.
const ROWS_PER_TASK: usize = if cfg!(test) { 3 } else { 4096 };

/// How often the push scatter checks the cancellation flag, in rows.
const ROWS_PER_CHECK: u64 = 1 << 16;

/// A semiring's `(⊕, ⊗)` fixed at compile time, so each product's inner
/// loop is specialised to its semiring.
pub(super) trait Ops<E>: Sync {
    /// `a ⊕ b`.
    fn add(a: E, b: E) -> Result<E, ArithmeticError>;

    /// `⊗(weight, value)`; the weight is cast only if `⊗` reads it.
    fn multiply(weight: f64, value: E) -> Result<E, ArithmeticError>;
}

/// The operators `BinaryOp::ALL[ADD]` and `BinaryOp::ALL[MULTIPLY]`.
pub(super) struct Fixed<const ADD: usize, const MULTIPLY: usize>;

impl<E: SemiringElement, const ADD: usize, const MULTIPLY: usize> Ops<E> for Fixed<ADD, MULTIPLY> {
    #[inline(always)]
    fn add(a: E, b: E) -> Result<E, ArithmeticError> {
        E::apply(BinaryOp::ALL[ADD], a, b)
    }

    #[inline(always)]
    fn multiply(weight: f64, value: E) -> Result<E, ArithmeticError> {
        let multiply = BinaryOp::ALL[MULTIPLY];
        let weight = match multiply {
            BinaryOp::Second | BinaryOp::Pair => E::default(),
            _ => E::from_weight(weight)?,
        };
        E::apply(multiply, weight, value)
    }
}

/// `$kernel::<$element, Fixed<⊕, ⊗>>(args)` for `$semiring`'s `⊕` and `⊗`.
macro_rules! specialised {
    ($semiring:expr, $kernel:ident::<$element:ty>($($arg:expr),*)) => {{
        use BinaryOp::{Any, First, Land, Lor, Max, Min, Pair, Plus, Second, Times};
        match $semiring {
            Semiring::PlusTimes => $kernel::<$element, Fixed<{ Plus as usize }, { Times as usize }>>($($arg),*),
            Semiring::MinPlus => $kernel::<$element, Fixed<{ Min as usize }, { Plus as usize }>>($($arg),*),
            Semiring::MaxPlus => $kernel::<$element, Fixed<{ Max as usize }, { Plus as usize }>>($($arg),*),
            Semiring::MaxTimes => $kernel::<$element, Fixed<{ Max as usize }, { Times as usize }>>($($arg),*),
            Semiring::MinTimes => $kernel::<$element, Fixed<{ Min as usize }, { Times as usize }>>($($arg),*),
            Semiring::LorLand => $kernel::<$element, Fixed<{ Lor as usize }, { Land as usize }>>($($arg),*),
            Semiring::AnyPair => $kernel::<$element, Fixed<{ Any as usize }, { Pair as usize }>>($($arg),*),
            Semiring::PlusPair => $kernel::<$element, Fixed<{ Plus as usize }, { Pair as usize }>>($($arg),*),
            Semiring::PlusFirst => $kernel::<$element, Fixed<{ Plus as usize }, { First as usize }>>($($arg),*),
            Semiring::PlusSecond => $kernel::<$element, Fixed<{ Plus as usize }, { Second as usize }>>($($arg),*),
            Semiring::MinFirst => $kernel::<$element, Fixed<{ Min as usize }, { First as usize }>>($($arg),*),
            Semiring::MinSecond => $kernel::<$element, Fixed<{ Min as usize }, { Second as usize }>>($($arg),*),
            Semiring::MaxSecond => $kernel::<$element, Fixed<{ Max as usize }, { Second as usize }>>($($arg),*),
        }
    }};
}

/// `y = A ⊗ x` row by row, in parallel.
pub(super) fn pull<E: SemiringElement>(
    semiring: Semiring,
    cancellation: &Cancellation,
    matrix: &Matrix<'_>,
    x: &Vector<E>,
    y: &mut [E],
    write: &Write<'_, E>,
) -> Result<(), ComputeError> {
    specialised!(semiring, pull_with::<E>(cancellation, matrix, x, y, write))
}

fn pull_with<E: SemiringElement, S: Ops<E>>(
    cancellation: &Cancellation,
    matrix: &Matrix<'_>,
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
            if width == 1 {
                // The SpMV fast path: a scalar sum, no row slices.
                let (values, sum) = (x.values(), &mut sum[0]);
                matrix.for_each_entry(row, &mut scratch, |column, weight| {
                    let product = S::multiply(weight, values[column as usize])?;
                    *sum = if present { S::add(*sum, product)? } else { product };
                    present = true;
                    Ok(())
                })?;
            } else {
                matrix.for_each_entry(row, &mut scratch, |column, weight| {
                    fold::<E, S>(&mut sum, present, weight, x.row(column))?;
                    present = true;
                    Ok(())
                })?;
            }
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
    semiring: Semiring,
    cancellation: &Cancellation,
    matrix: &Matrix<'_>,
    x: &Vector<E>,
    y: &mut [E],
    scratch: PushScratch<'_, E>,
    write: &Write<'_, E>,
) -> Result<(), ComputeError> {
    specialised!(semiring, push_with::<E>(cancellation, matrix, x, y, scratch, write))
}

fn push_with<E: SemiringElement, S: Ops<E>>(
    cancellation: &Cancellation,
    matrix: &Matrix<'_>,
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
            fold::<E, S>(sum, present[column], weight, values)?;
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
#[inline(always)]
fn fold<E: SemiringElement, S: Ops<E>>(
    sum: &mut [E],
    present: bool,
    weight: f64,
    values: &[E],
) -> Result<(), ArithmeticError> {
    for (sum, &value) in sum.iter_mut().zip(values) {
        let product = S::multiply(weight, value)?;
        *sum = if present { S::add(*sum, product)? } else { product };
    }
    Ok(())
}

/// How a row's result `t` is written to `y`.
#[derive(Debug)]
pub(super) struct Write<'a, E> {
    mask: Option<Mask<'a>>,
    replace: bool,
    accumulate: Option<BinaryOp>,
    identity: E,
}

impl<'a, E: SemiringElement> Write<'a, E> {
    pub(super) fn new(product: &Product<'a>) -> Self {
        Self {
            mask: product.mask,
            replace: product.replace,
            accumulate: product.accumulate,
            identity: E::identity(product.semiring.add()),
        }
    }

    /// Writes `t` (`None`: the row of `A` is empty) to `out`, row `row` of
    /// `y`.
    fn row(&self, row: DenseId, out: &mut [E], t: Option<&[E]>) -> Result<(), ArithmeticError> {
        if !self.mask.is_none_or(|mask| mask.selects(row)) {
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
