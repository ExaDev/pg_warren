//! `vector_map`: elementwise evaluation of the compiled subset of SQL
//! expressions over vectors (DESIGN.md §6.3).
//!
//! SQL semantics:
//! - Operators and functions behave as Postgres's do, errors included
//!   (see `arithmetic`). `CASE`, `COALESCE`, `AND` and `OR` evaluate an
//!   argument only for the rows Postgres would evaluate it for, so a guarded
//!   `1 / x` does not fail where `x = 0`.
//! - Vectors hold no NULL. An expression that is NULL for an element stores
//!   [`Value::NULL`] there: `NaN` in a float vector, an error in an `i64` or
//!   `bool` vector. A vector read back is never NULL (`NaN` stays `NaN`).
//! - When elements fail, the error is the first failing element's in
//!   row-major order, the order a row-at-a-time executor would meet it.
//!
//! Broadcasting: the result has the widest operand's width `d`; operands of
//! width 1 (and literals) repeat across each row, and any other width is an
//! error.

mod arithmetic;
mod column;
mod eval;
mod expr;

pub use expr::{
    AnyExpr, Arithmetic, BoolOp, Comparands, Comparison, Expr, F32Op, F64Op, Function, I64Op, Value,
};

use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use super::{AnyVector, ComputeError, ComputeMemory, Vector, Workers};
use crate::interrupts::{Cancelled, Interrupts};
use column::Column;
use eval::{eval, eval_any, Chunk, Eval};

/// The elements one task evaluates at once; small under test, so tests
/// cross chunk and row boundaries.
const CHUNK: usize = if cfg!(test) { 5 } else { 4096 };

/// Evaluates `expr` for every element of `id_space` rows, reading operand
/// vector `i` where the expression refers to vector `i`, and returns the
/// result as a new vector of the expression's type.
pub fn map<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    id_space: u64,
    expr: &AnyExpr,
    operands: &[&AnyVector],
) -> Result<AnyVector, ComputeError> {
    let width = operands.iter().map(|operand| operand.width()).max().unwrap_or(1);
    for operand in operands {
        if operand.id_space() != id_space {
            return Err(ComputeError::IdSpaceMismatch {
                expected: id_space,
                found: operand.id_space(),
            });
        }
        if operand.width() != 1 && operand.width() != width {
            return Err(ComputeError::WidthMismatch { expected: width, found: operand.width() });
        }
    }
    // Evaluating no elements visits every node, checking operand references.
    eval_any(expr, &Chunk { operands, width, start: 0, len: 0 }, &[])?;
    let mut task = Task { workers, memory, interrupts, id_space, width, operands };
    Ok(match expr {
        AnyExpr::Bool(expr) => task.run(expr)?.into(),
        AnyExpr::I64(expr) => task.run(expr)?.into(),
        AnyExpr::F32(expr) => task.run(expr)?.into(),
        AnyExpr::F64(expr) => task.run(expr)?.into(),
    })
}

/// A checked map, ready to run at its result type.
struct Task<'t, I> {
    workers: &'t Workers,
    memory: &'t ComputeMemory,
    interrupts: &'t mut I,
    id_space: u64,
    width: usize,
    operands: &'t [&'t AnyVector],
}

/// An element that failed, by its row-major position.
struct Failure {
    position: usize,
    error: ComputeError,
}

impl<I: Interrupts> Task<'_, I> {
    /// Evaluates chunks in parallel. A chunk starting after a known failure
    /// is skipped; the failure reported is the first by position.
    fn run<T: Eval>(&mut self, expr: &Expr<T>) -> Result<Vector<T>, ComputeError> {
        let mut output = Vector::filled(self.memory, self.id_space, self.width, T::default())?;
        let (operands, width) = (self.operands, self.width);
        let first_failure = AtomicUsize::new(usize::MAX);
        let values = output.values_mut();
        self.workers.run(self.interrupts, |cancellation| {
            let failures: Vec<Failure> = values
                .par_chunks_mut(CHUNK)
                .enumerate()
                .filter_map(|(index, out)| {
                    let start = index * CHUNK;
                    if start > first_failure.load(Ordering::Relaxed) {
                        return None;
                    }
                    if cancellation.is_cancelled() {
                        return Some(Failure { position: start, error: Cancelled.into() });
                    }
                    let chunk = Chunk { operands, width, start, len: out.len() };
                    let failure = evaluate(expr, &chunk, out).err()?;
                    first_failure.fetch_min(failure.position, Ordering::Relaxed);
                    Some(failure)
                })
                .collect();
            match failures.into_iter().min_by_key(|failure| failure.position) {
                Some(failure) => Err(failure.error),
                None => Ok(()),
            }
        })?;
        Ok(output)
    }
}

/// Evaluates `chunk` into `out`. If any element fails, evaluates the chunk
/// again one element at a time to find the first failing element.
fn evaluate<T: Eval>(expr: &Expr<T>, chunk: &Chunk<'_>, out: &mut [T]) -> Result<(), Failure> {
    let whole = eval(expr, chunk, &vec![true; chunk.len]).and_then(|column| store(&column, out));
    if whole.is_ok() {
        return Ok(());
    }
    for (offset, out) in out.chunks_mut(1).enumerate() {
        let element = Chunk { start: chunk.start + offset, len: 1, ..*chunk };
        eval(expr, &element, &[true])
            .and_then(|column| store(&column, out))
            .map_err(|error| Failure { position: element.start, error })?;
    }
    Ok(())
}

/// Writes `column` to `out`, storing NULL as [`Value::NULL`].
fn store<T: Value>(column: &Column<T>, out: &mut [T]) -> Result<(), ComputeError> {
    for ((out, &value), &null) in out.iter_mut().zip(&column.values).zip(&column.nulls) {
        *out = match null {
            true => T::NULL.ok_or(ComputeError::NullResult { dtype: T::DTYPE })?,
            false => value,
        };
    }
    Ok(())
}

// Not under Miri: the compute pool's work stealing runs through
// crossbeam-epoch, which Miri's Stacked Borrows model rejects (a known
// report against crossbeam-epoch 0.9, not this crate, which has no unsafe).
#[cfg(test)]
#[cfg(not(miri))]
mod tests;
