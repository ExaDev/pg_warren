//! The executor fallback of `vector_map` (DESIGN.md §6.3): the analysed
//! expression evaluated element by element through Postgres's expression
//! machinery (`ExecInitExpr` once, `ExecEvalExpr` per element, the
//! per-element memory reset every batch, JIT-compiled when the `jit`
//! settings would compile a plan of the same cost). Main thread only.
//!
//! Each element is a virtual tuple of the synthetic row: the referenced
//! vectors' values there (a width-1 vector repeats across each row), the
//! other columns NULL. A NULL result is stored as the compiled path stores
//! it (`compute::Value::NULL`), and the first failing element, in row-major
//! order, raises its error, so both paths agree.

use std::cell::Cell;
use std::mem::size_of;

use pgrx::prelude::*;
use warren_core::compute::{AnyVector, ComputeError, ComputeMemory, Dtype, Value, Vector};

use super::parse::Analysed;
use crate::compute_sql::ComputeSqlError;

/// Elements evaluated between memory resets and interrupt checks.
const BATCH: usize = 4096;

thread_local! {
    /// Whether the last executor evaluation was JIT-compiled.
    static JIT_COMPILED: Cell<bool> = const { Cell::new(false) };
}

/// Whether the last executor evaluation in this backend was JIT-compiled.
#[cfg(any(test, feature = "pg_test"))]
pub(crate) fn last_was_jit_compiled() -> bool {
    JIT_COMPILED.get()
}

/// The columns (0-based) `analysed` refers to, ascending.
pub(crate) fn referenced(analysed: &Analysed) -> Vec<usize> {
    let mut attributes = std::ptr::null_mut();
    // SAFETY: `analysed.expr` is a valid expression tree; pull_varattnos
    // adds the attribute numbers of its varno-1 Vars to a new Bitmapset.
    unsafe { pg_sys::pull_varattnos(analysed.expr, 1, &mut attributes) };
    let mut columns = Vec::new();
    let mut member = -1;
    loop {
        // SAFETY: `attributes` is NULL (empty) or the Bitmapset made above.
        member = unsafe { pg_sys::bms_next_member(attributes, member) };
        if member < 0 {
            return columns;
        }
        let attribute = member + pg_sys::FirstLowInvalidHeapAttributeNumber;
        if let Ok(column) = usize::try_from(attribute - 1) {
            columns.push(column);
        }
    }
}

/// An element of a vector as a datum of its SQL type.
fn datum(vector: &AnyVector, index: usize) -> pg_sys::Datum {
    let bits = match vector {
        AnyVector::F64(vector) => vector.values()[index].to_bits() as usize,
        AnyVector::F32(vector) => vector.values()[index].to_bits() as usize,
        AnyVector::I64(vector) => vector.values()[index] as usize,
        AnyVector::Bool(vector) => usize::from(vector.values()[index]),
    };
    pg_sys::Datum::from(bits)
}

/// A result type the executor stores.
trait Stored: Value {
    fn from_datum(datum: pg_sys::Datum) -> Self;
}

impl Stored for f64 {
    fn from_datum(datum: pg_sys::Datum) -> Self {
        f64::from_bits(datum.value() as u64)
    }
}

impl Stored for f32 {
    fn from_datum(datum: pg_sys::Datum) -> Self {
        f32::from_bits(datum.value() as u32)
    }
}

impl Stored for i64 {
    fn from_datum(datum: pg_sys::Datum) -> Self {
        datum.value() as i64
    }
}

impl Stored for bool {
    fn from_datum(datum: pg_sys::Datum) -> Self {
        datum.value() != 0
    }
}

/// The JIT flags the planner would set for a plan costing `cost`.
fn jit_flags(cost: f64) -> i32 {
    // SAFETY: reads the backend's JIT and cost settings; main thread.
    unsafe {
        if !pg_sys::jit_enabled || !pg_sys::jit_expressions || cost < pg_sys::jit_above_cost {
            return 0;
        }
        let mut flags = pg_sys::PGJIT_PERFORM | pg_sys::PGJIT_EXPR;
        if cost >= pg_sys::jit_optimize_above_cost {
            flags |= pg_sys::PGJIT_OPT3;
        }
        if cost >= pg_sys::jit_inline_above_cost {
            flags |= pg_sys::PGJIT_INLINE;
        }
        flags as i32
    }
}

/// Evaluates `analysed` at every element of `id_space` rows, reading
/// column `columns[i]` from `operands[i]`, into a vector charged to
/// `memory`.
pub(crate) fn evaluate(
    analysed: &Analysed,
    columns: &[usize],
    operands: &[&AnyVector],
    id_space: u64,
    memory: &ComputeMemory,
) -> Result<AnyVector, ComputeSqlError> {
    let width = operands.iter().map(|operand| operand.width()).max().unwrap_or(1);
    for operand in operands {
        if operand.id_space() != id_space {
            let found = operand.id_space();
            return Err(ComputeError::IdSpaceMismatch { expected: id_space, found }.into());
        }
        if operand.width() != 1 && operand.width() != width {
            return Err(
                ComputeError::WidthMismatch { expected: width, found: operand.width() }.into()
            );
        }
    }
    let evaluation = Evaluation { analysed, columns, operands, width };
    Ok(match analysed.dtype {
        Dtype::F64 => evaluation.run::<f64>(Vector::filled(memory, id_space, width, 0.0)?)?.into(),
        Dtype::F32 => evaluation.run::<f32>(Vector::filled(memory, id_space, width, 0.0)?)?.into(),
        Dtype::I64 => evaluation.run::<i64>(Vector::filled(memory, id_space, width, 0)?)?.into(),
        Dtype::Bool => {
            evaluation.run::<bool>(Vector::filled(memory, id_space, width, false)?)?.into()
        }
    })
}

/// A checked evaluation.
struct Evaluation<'a> {
    analysed: &'a Analysed,
    columns: &'a [usize],
    operands: &'a [&'a AnyVector],
    width: usize,
}

impl Evaluation<'_> {
    fn run<T: Stored>(&self, mut output: Vector<T>) -> Result<Vector<T>, ComputeSqlError> {
        let natts = i32::try_from(self.analysed.columns.len().max(1)).unwrap_or(i32::MAX);
        let len = output.values().len();
        let values = output.values_mut()?;
        let cost = len as f64 * {
            // SAFETY: reads a planner cost setting; main thread.
            unsafe { pg_sys::cpu_operator_cost }
        };
        // SAFETY: an executor state, a standalone parent plan node for JIT,
        // a tuple descriptor of the synthetic row and a virtual slot over
        // it, all in the current memory context; ExecInitExpr compiles the
        // analysed expression (which has no SubPlans, aggregates or
        // window functions) against that parent. FreeExecutorState below,
        // or the (sub)transaction abort on an ERROR, releases them,
        // including any JIT context.
        let (estate, econtext, state, slot) = unsafe {
            let estate = pg_sys::CreateExecutorState();
            (*estate).es_jit_flags = jit_flags(cost);
            let parent =
                pg_sys::palloc0(size_of::<pg_sys::ResultState>()).cast::<pg_sys::ResultState>();
            (*parent).ps.type_ = pg_sys::NodeTag::T_ResultState;
            (*parent).ps.state = estate;
            let descriptor = pg_sys::CreateTemplateTupleDesc(natts);
            for (number, column) in (1..).zip(&self.analysed.columns) {
                let oid = match column.dtype {
                    Dtype::F64 => pg_sys::FLOAT8OID,
                    Dtype::F32 => pg_sys::FLOAT4OID,
                    Dtype::I64 => pg_sys::INT8OID,
                    Dtype::Bool => pg_sys::BOOLOID,
                };
                pg_sys::TupleDescInitEntry(descriptor, number, std::ptr::null(), oid, -1, 0);
            }
            if self.analysed.columns.is_empty() {
                pg_sys::TupleDescInitEntry(descriptor, 1, std::ptr::null(), pg_sys::BOOLOID, -1, 0);
            }
            let slot = pg_sys::MakeSingleTupleTableSlot(descriptor, &pg_sys::TTSOpsVirtual);
            let econtext = pg_sys::CreateExprContext(estate);
            (*econtext).ecxt_scantuple = slot;
            let state = pg_sys::ExecInitExpr(self.analysed.expr.cast(), &mut (*parent).ps);
            JIT_COMPILED.set(!(*estate).es_jit.is_null());
            (estate, econtext, state, slot)
        };
        let outcome = (|| {
            for start in (0..len).step_by(BATCH) {
                check_for_interrupts!();
                // SAFETY: the per-tuple context holds only the previous batch's
                // results, which have been copied out.
                unsafe { pg_sys::MemoryContextReset((*econtext).ecxt_per_tuple_memory) };
                for (position, value) in (start..).zip(&mut values[start..len.min(start + BATCH)]) {
                    // SAFETY: `slot` has one value per column of its descriptor;
                    // the unreferenced ones are NULL.
                    unsafe {
                        pg_sys::ExecClearTuple(slot);
                        let natts = natts as usize;
                        let datums = std::slice::from_raw_parts_mut((*slot).tts_values, natts);
                        let nulls = std::slice::from_raw_parts_mut((*slot).tts_isnull, natts);
                        nulls.fill(true);
                        for (&column, operand) in self.columns.iter().zip(self.operands) {
                            let index = match operand.width() == self.width {
                                true => position,
                                false => position / self.width,
                            };
                            datums[column] = datum(operand, index);
                            nulls[column] = false;
                        }
                        pg_sys::ExecStoreVirtualTuple(slot);
                    }
                    let mut null = false;
                    // SAFETY: `state` was compiled for this slot's descriptor;
                    // an ERROR unwinds out of the loop.
                    let result =
                        unsafe { pg_sys::ExecEvalExprSwitchContext(state, econtext, &mut null) };
                    *value = match null {
                        true => T::NULL.ok_or(ComputeError::NullResult { dtype: T::DTYPE })?,
                        false => T::from_datum(result),
                    };
                }
            }
            Ok::<_, ComputeSqlError>(())
        })();
        // SAFETY: frees the slot and the executor state made above.
        unsafe {
            pg_sys::ExecDropSingleTupleTableSlot(slot);
            pg_sys::FreeExecutorState(estate);
        }
        outcome.map(|()| output)
    }
}
