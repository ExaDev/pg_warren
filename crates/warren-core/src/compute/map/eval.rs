//! Columnar evaluation of the IR over a chunk of elements.
//!
//! Each node is evaluated for a whole chunk at once, under an `active`
//! mask: the elements SQL would evaluate it for. `CASE`, `COALESCE`, `AND`
//! and `OR` narrow the mask for the arguments SQL evaluates lazily, so an
//! error can only come from an element SQL would have evaluated. Every node
//! is visited even when no element is active, so evaluating a chunk of no
//! elements checks every operand reference.

use std::cmp::Ordering;

use super::arithmetic::{self, SqlFloat};
use super::column::{AnyColumn, Column};
use super::{
    AnyExpr, Arithmetic, BoolOp, Comparands, Comparison, Expr, F32Op, F64Op, I64Op, Value,
};
use crate::compute::cast::SqlCast;
use crate::compute::{AnyVector, ArithmeticError, ComputeError};

/// The elements `start..start + len` of the result, row-major over rows of
/// `width`, and the operand vectors they read.
#[derive(Debug, Clone, Copy)]
pub(super) struct Chunk<'a> {
    pub(super) operands: &'a [&'a AnyVector],
    pub(super) width: usize,
    pub(super) start: usize,
    pub(super) len: usize,
}

impl Chunk<'_> {
    /// Operand `index` at each element; a width-1 operand repeats across
    /// each row.
    fn load<T: Value>(&self, index: usize) -> Result<Column<T>, ComputeError> {
        let operand = self
            .operands
            .get(index)
            .ok_or(ComputeError::UnknownOperand { index, operands: self.operands.len() })?;
        let vector = T::vector(operand).ok_or(ComputeError::OperandType {
            index,
            expected: T::DTYPE,
            found: operand.dtype(),
        })?;
        let (values, width) = (vector.values(), self.width);
        let values = (self.start..self.start + self.len)
            .map(|p| if vector.width() == width { values[p] } else { values[p / width] })
            .collect();
        Ok(Column { values, nulls: vec![false; self.len] })
    }
}

/// A value type the evaluator computes.
pub(super) trait Eval: Value {
    /// The operations particular to this type.
    fn op(op: &Self::Op, chunk: &Chunk<'_>, active: &[bool]) -> Result<Column<Self>, ComputeError>;

    /// A column of another type, cast to this one.
    fn cast(column: &AnyColumn, active: &[bool]) -> Result<Column<Self>, ComputeError>;
}

/// `expr` at each element of `chunk`.
pub(super) fn eval<T: Eval>(
    expr: &Expr<T>,
    chunk: &Chunk<'_>,
    active: &[bool],
) -> Result<Column<T>, ComputeError> {
    match expr {
        Expr::Literal(value) => Ok(Column::constant(chunk.len, *value)),
        Expr::Vector(index) => chunk.load(*index),
        Expr::Cast(inner) => T::cast(&eval_any(inner, chunk, active)?, active),
        Expr::Case { when, otherwise } => {
            let mut result = Column::constant(chunk.len, None);
            let mut remaining = active.to_vec();
            for (condition, value) in when {
                let condition = eval(condition, chunk, &remaining)?;
                let matched: Vec<bool> = (0..chunk.len)
                    .map(|i| remaining[i] && !condition.nulls[i] && condition.values[i])
                    .collect();
                result.assign(&eval(value, chunk, &matched)?, &matched);
                remaining.iter_mut().zip(&matched).for_each(|(r, m)| *r &= !m);
            }
            if let Some(otherwise) = otherwise {
                result.assign(&eval(otherwise, chunk, &remaining)?, &remaining);
            }
            Ok(result)
        }
        Expr::NullIf(a, b) => {
            let (mut a, b) = (eval(a, chunk, active)?, eval(b, chunk, active)?);
            for i in 0..chunk.len {
                if !a.nulls[i] && !b.nulls[i] && a.values[i].sql_cmp(b.values[i]).is_eq() {
                    a.nulls[i] = true;
                }
            }
            Ok(a)
        }
        Expr::Coalesce(args) => {
            let mut result = Column::constant(chunk.len, None);
            let mut remaining = active.to_vec();
            for arg in args {
                let value = eval(arg, chunk, &remaining)?;
                let found: Vec<bool> =
                    (0..chunk.len).map(|i| remaining[i] && !value.nulls[i]).collect();
                result.assign(&value, &found);
                remaining.iter_mut().zip(&found).for_each(|(r, f)| *r &= !f);
            }
            Ok(result)
        }
        Expr::Least(args) => extreme(args, Ordering::Less, chunk, active),
        Expr::Greatest(args) => extreme(args, Ordering::Greater, chunk, active),
        Expr::Op(op) => T::op(op, chunk, active),
    }
}

/// `LEAST` (`want = Less`) or `GREATEST`: every argument is evaluated; the
/// first of equal values wins and NULLs are skipped.
fn extreme<T: Eval>(
    args: &[Expr<T>],
    want: Ordering,
    chunk: &Chunk<'_>,
    active: &[bool],
) -> Result<Column<T>, ComputeError> {
    let mut result = Column::constant(chunk.len, None);
    for arg in args {
        let value = eval(arg, chunk, active)?;
        for i in 0..chunk.len {
            let better = result.nulls[i] || value.values[i].sql_cmp(result.values[i]) == want;
            if !value.nulls[i] && better {
                result.values[i] = value.values[i];
                result.nulls[i] = false;
            }
        }
    }
    Ok(result)
}

/// An expression of any type at each element of `chunk`.
pub(super) fn eval_any(
    expr: &AnyExpr,
    chunk: &Chunk<'_>,
    active: &[bool],
) -> Result<AnyColumn, ComputeError> {
    Ok(match expr {
        AnyExpr::Bool(expr) => AnyColumn::Bool(eval(expr, chunk, active)?),
        AnyExpr::I64(expr) => AnyColumn::I64(eval(expr, chunk, active)?),
        AnyExpr::F32(expr) => AnyColumn::F32(eval(expr, chunk, active)?),
        AnyExpr::F64(expr) => AnyColumn::F64(eval(expr, chunk, active)?),
    })
}

/// `column` cast to `T` as SQL casts.
fn cast<T: Copy + Default>(column: &AnyColumn, active: &[bool]) -> Result<Column<T>, ComputeError>
where
    bool: SqlCast<T>,
    i64: SqlCast<T>,
    f32: SqlCast<T>,
    f64: SqlCast<T>,
{
    match column {
        AnyColumn::Bool(column) => column.map(active, SqlCast::sql_cast),
        AnyColumn::I64(column) => column.map(active, SqlCast::sql_cast),
        AnyColumn::F32(column) => column.map(active, SqlCast::sql_cast),
        AnyColumn::F64(column) => column.map(active, SqlCast::sql_cast),
    }
}

/// `a op b` in SQL order.
fn compare<T: Eval>(
    comparison: Comparison,
    a: &Expr<T>,
    b: &Expr<T>,
    chunk: &Chunk<'_>,
    active: &[bool],
) -> Result<Column<bool>, ComputeError> {
    let (a, b) = (eval(a, chunk, active)?, eval(b, chunk, active)?);
    a.zip(&b, active, |a, b| {
        let order = a.sql_cmp(b);
        Ok::<_, ComputeError>(match comparison {
            Comparison::Eq => order.is_eq(),
            Comparison::Ne => order.is_ne(),
            Comparison::Lt => order.is_lt(),
            Comparison::Le => order.is_le(),
            Comparison::Gt => order.is_gt(),
            Comparison::Ge => order.is_ge(),
        })
    })
}

/// `AND` (`stop = false`) or `OR` (`stop = true`): arguments left to right,
/// each only where no earlier argument was `stop`; then `stop` if any
/// argument was, else NULL if any was NULL, else `!stop`.
fn connective(
    args: &[Expr<bool>],
    stop: bool,
    chunk: &Chunk<'_>,
    active: &[bool],
) -> Result<Column<bool>, ComputeError> {
    let mut result = Column::constant(chunk.len, Some(!stop));
    let mut remaining = active.to_vec();
    for arg in args {
        let value = eval(arg, chunk, &remaining)?;
        for (i, remaining) in remaining.iter_mut().enumerate().filter(|(_, r)| **r) {
            if value.nulls[i] {
                result.nulls[i] = true;
            } else if value.values[i] == stop {
                result.values[i] = stop;
                result.nulls[i] = false;
                *remaining = false;
            }
        }
    }
    Ok(result)
}

fn ok<T>(value: T) -> Result<T, ArithmeticError> {
    Ok(value)
}

impl Eval for bool {
    fn op(op: &BoolOp, chunk: &Chunk<'_>, active: &[bool]) -> Result<Column<bool>, ComputeError> {
        match op {
            BoolOp::Compare(comparison, comparands) => match comparands.as_ref() {
                Comparands::Bool(a, b) => compare(*comparison, a, b, chunk, active),
                Comparands::I64(a, b) => compare(*comparison, a, b, chunk, active),
                Comparands::F32(a, b) => compare(*comparison, a, b, chunk, active),
                Comparands::F64(a, b) => compare(*comparison, a, b, chunk, active),
            },
            BoolOp::Not(a) => eval(a, chunk, active)?.map(active, |a| ok(!a)),
            BoolOp::And(args) => connective(args, false, chunk, active),
            BoolOp::Or(args) => connective(args, true, chunk, active),
        }
    }

    fn cast(column: &AnyColumn, active: &[bool]) -> Result<Column<bool>, ComputeError> {
        cast(column, active)
    }
}

impl Eval for i64 {
    fn op(op: &I64Op, chunk: &Chunk<'_>, active: &[bool]) -> Result<Column<i64>, ComputeError> {
        match op {
            I64Op::Negate(a) => eval(a, chunk, active)?.map(active, arithmetic::int_negate),
            I64Op::Abs(a) => eval(a, chunk, active)?.map(active, arithmetic::int_abs),
            I64Op::Modulo(a, b) => {
                let (a, b) = (eval(a, chunk, active)?, eval(b, chunk, active)?);
                a.zip(&b, active, arithmetic::int_modulo)
            }
            I64Op::Arithmetic(op, a, b) => {
                let (a, b) = (eval(a, chunk, active)?, eval(b, chunk, active)?);
                a.zip(&b, active, |a, b| arithmetic::int(*op, a, b))
            }
        }
    }

    fn cast(column: &AnyColumn, active: &[bool]) -> Result<Column<i64>, ComputeError> {
        cast(column, active)
    }
}

/// `-a` on floats: no check, as `float8um`.
fn negate<F: Eval + SqlFloat>(
    a: &Expr<F>,
    chunk: &Chunk<'_>,
    active: &[bool],
) -> Result<Column<F>, ComputeError> {
    eval(a, chunk, active)?.map(active, |a| ok(-a))
}

/// `a op b` on floats.
fn float_arithmetic<F: Eval + SqlFloat>(
    op: Arithmetic,
    a: &Expr<F>,
    b: &Expr<F>,
    chunk: &Chunk<'_>,
    active: &[bool],
) -> Result<Column<F>, ComputeError> {
    let (a, b) = (eval(a, chunk, active)?, eval(b, chunk, active)?);
    a.zip(&b, active, |a, b| arithmetic::float(op, a, b))
}

impl Eval for f32 {
    fn op(op: &F32Op, chunk: &Chunk<'_>, active: &[bool]) -> Result<Column<f32>, ComputeError> {
        match op {
            F32Op::Negate(a) => negate(a, chunk, active),
            F32Op::Arithmetic(op, a, b) => float_arithmetic(*op, a, b, chunk, active),
            F32Op::Abs(a) => eval(a, chunk, active)?.map(active, |a| ok(a.abs())),
        }
    }

    fn cast(column: &AnyColumn, active: &[bool]) -> Result<Column<f32>, ComputeError> {
        cast(column, active)
    }
}

impl Eval for f64 {
    fn op(op: &F64Op, chunk: &Chunk<'_>, active: &[bool]) -> Result<Column<f64>, ComputeError> {
        match op {
            F64Op::Negate(a) => negate(a, chunk, active),
            F64Op::Arithmetic(op, a, b) => float_arithmetic(*op, a, b, chunk, active),
            F64Op::Call(function, a) => {
                eval(a, chunk, active)?.map(active, |a| arithmetic::call(*function, a))
            }
            F64Op::Power(a, b) => {
                let (a, b) = (eval(a, chunk, active)?, eval(b, chunk, active)?);
                a.zip(&b, active, arithmetic::power)
            }
        }
    }

    fn cast(column: &AnyColumn, active: &[bool]) -> Result<Column<f64>, ComputeError> {
        cast(column, active)
    }
}
