//! The no-NULL path: evaluating an expression that is never NULL over a
//! chunk, without NULL flags (DESIGN.md §6.3).
//!
//! Vectors hold no NULL, so an expression built only from vectors,
//! non-NULL literals, casts, arithmetic, comparisons, `NOT` and `CASE`
//! with an `ELSE` over those is never NULL ([`covers`]). Here such an
//! expression reads operands of the result's width in place and runs each
//! node as one loop over the chunk, its operator chosen outside the loop.
//! The arithmetic is the columnar path's, on the same values in the same
//! order, so the results are the same bits. `CASE` narrows the active
//! elements as the columnar path does, and an error counts only at an
//! active element. A failure is reported, not described: the caller then
//! evaluates the chunk on the columnar path, which finds the first failing
//! element and its error.

use std::borrow::Cow;

use super::arithmetic::{self, SqlFloat};
use super::eval::{Chunk, Eval};
use super::{AnyExpr, Arithmetic, BoolOp, Comparands, Comparison, Expr, F32Op, F64Op, I64Op};
use crate::compute::cast::SqlCast;
use crate::compute::{ArithmeticError, Element};

/// Some active element failed (or an operand reference is invalid).
#[derive(Debug)]
pub(super) struct Failed;

/// A node's values over a chunk: operand values borrowed in place, or
/// computed.
pub(super) type Values<'a, T> = Result<Cow<'a, [T]>, Failed>;

/// The elements that count; `None`: all of them.
type Active<'m> = Option<&'m [bool]>;

/// A value type the no-NULL path computes.
pub(super) trait NotNull: Eval {
    /// Whether `op` is never NULL, given operands that are not.
    fn covers_op(op: &Self::Op) -> bool;

    /// `op` at each element of `chunk`.
    fn op<'a>(op: &Self::Op, chunk: &Chunk<'a>, active: Active<'_>) -> Values<'a, Self>;

    /// `from` at each element of `chunk`, cast to this type.
    fn cast<'a>(from: &AnyExpr, chunk: &Chunk<'a>, active: Active<'_>) -> Values<'a, Self>;
}

/// Whether `expr` is never NULL, so the no-NULL path evaluates it.
pub(super) fn covers<T: NotNull>(expr: &Expr<T>) -> bool {
    match expr {
        Expr::Literal(value) => value.is_some(),
        Expr::Vector(_) => true,
        Expr::Cast(from) => covers_any(from),
        Expr::Case { when, otherwise: Some(otherwise) } => {
            when.iter().all(|(condition, value)| covers(condition) && covers(value))
                && covers(otherwise)
        }
        Expr::Op(op) => T::covers_op(op),
        Expr::Case { otherwise: None, .. }
        | Expr::NullIf(..)
        | Expr::Coalesce(_)
        | Expr::Least(_)
        | Expr::Greatest(_) => false,
    }
}

fn covers_any(expr: &AnyExpr) -> bool {
    match expr {
        AnyExpr::Bool(expr) => covers(expr),
        AnyExpr::I64(expr) => covers(expr),
        AnyExpr::F32(expr) => covers(expr),
        AnyExpr::F64(expr) => covers(expr),
    }
}

/// `expr` at each element of `chunk`, or [`Failed`] if an active element
/// fails. Only for expressions [`covers`] accepts.
pub(super) fn eval<'a, T: NotNull>(
    expr: &Expr<T>,
    chunk: &Chunk<'a>,
    active: Active<'_>,
) -> Values<'a, T> {
    match expr {
        Expr::Literal(Some(value)) => Ok(Cow::Owned(vec![*value; chunk.len])),
        Expr::Vector(index) => load(chunk, *index),
        Expr::Cast(from) => <T as NotNull>::cast(from, chunk, active),
        Expr::Case { when, otherwise: Some(otherwise) } => case(when, otherwise, chunk, active),
        Expr::Op(op) => <T as NotNull>::op(op, chunk, active),
        _ => Err(Failed),
    }
}

/// Operand `index` at each element: in place when it has the result's
/// width, else repeated across each row.
fn load<'a, T: Element>(chunk: &Chunk<'a>, index: usize) -> Values<'a, T> {
    let operands: &'a [&'a _] = chunk.operands;
    let vector = operands.get(index).and_then(|&operand| T::vector(operand)).ok_or(Failed)?;
    let (values, width) = (vector.values(), chunk.width);
    let range = chunk.start..chunk.start + chunk.len;
    match vector.width() == width {
        true => values.get(range).map(Cow::Borrowed).ok_or(Failed),
        false => Ok(Cow::Owned(range.map(|p| values[p / width]).collect())),
    }
}

/// `CASE`: each condition where no earlier one matched, each result where
/// its condition matched, and `ELSE` where none did.
fn case<'a, T: NotNull>(
    when: &[(Expr<bool>, Expr<T>)],
    otherwise: &Expr<T>,
    chunk: &Chunk<'a>,
    active: Active<'_>,
) -> Values<'a, T> {
    let mut remaining = active.map_or_else(|| vec![true; chunk.len], <[bool]>::to_vec);
    let mut result = vec![T::default(); chunk.len];
    for (condition, value) in when {
        let condition = eval(condition, chunk, Some(&remaining))?;
        let matched: Vec<bool> = remaining.iter().zip(&*condition).map(|(&r, &c)| r && c).collect();
        assign(&mut result, &eval(value, chunk, Some(&matched))?, &matched);
        remaining.iter_mut().zip(&matched).for_each(|(r, m)| *r &= !m);
    }
    assign(&mut result, &eval(otherwise, chunk, Some(&remaining))?, &remaining);
    Ok(Cow::Owned(result))
}

fn assign<T: Copy>(result: &mut [T], from: &[T], which: &[bool]) {
    for ((result, &from), _) in result.iter_mut().zip(from).zip(which).filter(|(_, &set)| set) {
        *result = from;
    }
}

/// `f(a)` at each element; an error counts only where active.
fn unary<'a, A: Copy, T: Clone + Default>(
    a: &[A],
    active: Active<'_>,
    f: impl Fn(A) -> Result<T, ArithmeticError>,
) -> Values<'a, T> {
    let mut out = Vec::with_capacity(a.len());
    for (i, &a) in a.iter().enumerate() {
        out.push(settled(f(a), i, active)?);
    }
    Ok(Cow::Owned(out))
}

/// `f(a, b)` at each element; an error counts only where active.
fn binary<'a, A: Copy, B: Copy, T: Clone + Default>(
    a: &[A],
    b: &[B],
    active: Active<'_>,
    f: impl Fn(A, B) -> Result<T, ArithmeticError>,
) -> Values<'a, T> {
    let mut out = Vec::with_capacity(a.len());
    for (i, (&a, &b)) in a.iter().zip(b).enumerate() {
        out.push(settled(f(a, b), i, active)?);
    }
    Ok(Cow::Owned(out))
}

/// Element `i`'s value; a placeholder if it failed but is inactive.
#[inline(always)]
fn settled<T: Default>(
    value: Result<T, ArithmeticError>,
    i: usize,
    active: Active<'_>,
) -> Result<T, Failed> {
    match value {
        Ok(value) => Ok(value),
        Err(_) if active.is_some_and(|active| !active[i]) => Ok(T::default()),
        Err(_) => Err(Failed),
    }
}

fn ok<T>(value: T) -> Result<T, ArithmeticError> {
    Ok(value)
}

/// `from` cast to `T` as SQL casts.
fn cast<'a, T: Clone + Default>(
    from: &AnyExpr,
    chunk: &Chunk<'a>,
    active: Active<'_>,
) -> Values<'a, T>
where
    bool: SqlCast<T>,
    i64: SqlCast<T>,
    f32: SqlCast<T>,
    f64: SqlCast<T>,
{
    match from {
        AnyExpr::Bool(from) => unary(&eval(from, chunk, active)?, active, SqlCast::sql_cast),
        AnyExpr::I64(from) => unary(&eval(from, chunk, active)?, active, SqlCast::sql_cast),
        AnyExpr::F32(from) => unary(&eval(from, chunk, active)?, active, SqlCast::sql_cast),
        AnyExpr::F64(from) => unary(&eval(from, chunk, active)?, active, SqlCast::sql_cast),
    }
}

/// `a op b` in SQL order.
fn compare<'a, T: NotNull>(
    comparison: Comparison,
    a: &Expr<T>,
    b: &Expr<T>,
    chunk: &Chunk<'a>,
    active: Active<'_>,
) -> Values<'a, bool> {
    let (a, b) = (eval(a, chunk, active)?, eval(b, chunk, active)?);
    match comparison {
        Comparison::Eq => binary(&a, &b, active, |a: T, b| ok(a.sql_cmp(b).is_eq())),
        Comparison::Ne => binary(&a, &b, active, |a: T, b| ok(a.sql_cmp(b).is_ne())),
        Comparison::Lt => binary(&a, &b, active, |a: T, b| ok(a.sql_cmp(b).is_lt())),
        Comparison::Le => binary(&a, &b, active, |a: T, b| ok(a.sql_cmp(b).is_le())),
        Comparison::Gt => binary(&a, &b, active, |a: T, b| ok(a.sql_cmp(b).is_gt())),
        Comparison::Ge => binary(&a, &b, active, |a: T, b| ok(a.sql_cmp(b).is_ge())),
    }
}

fn covers_comparands(comparands: &Comparands) -> bool {
    match comparands {
        Comparands::Bool(a, b) => covers(a) && covers(b),
        Comparands::I64(a, b) => covers(a) && covers(b),
        Comparands::F32(a, b) => covers(a) && covers(b),
        Comparands::F64(a, b) => covers(a) && covers(b),
    }
}

impl NotNull for bool {
    fn covers_op(op: &BoolOp) -> bool {
        match op {
            BoolOp::Compare(_, comparands) => covers_comparands(comparands),
            BoolOp::Not(a) => covers(a),
            BoolOp::And(_) | BoolOp::Or(_) => false,
        }
    }

    fn op<'a>(op: &BoolOp, chunk: &Chunk<'a>, active: Active<'_>) -> Values<'a, bool> {
        match op {
            BoolOp::Compare(comparison, comparands) => match comparands.as_ref() {
                Comparands::Bool(a, b) => compare(*comparison, a, b, chunk, active),
                Comparands::I64(a, b) => compare(*comparison, a, b, chunk, active),
                Comparands::F32(a, b) => compare(*comparison, a, b, chunk, active),
                Comparands::F64(a, b) => compare(*comparison, a, b, chunk, active),
            },
            BoolOp::Not(a) => unary(&eval(a, chunk, active)?, active, |a: bool| ok(!a)),
            BoolOp::And(_) | BoolOp::Or(_) => Err(Failed),
        }
    }

    fn cast<'a>(from: &AnyExpr, chunk: &Chunk<'a>, active: Active<'_>) -> Values<'a, bool> {
        cast(from, chunk, active)
    }
}

impl NotNull for i64 {
    fn covers_op(op: &I64Op) -> bool {
        match op {
            I64Op::Negate(a) | I64Op::Abs(a) => covers(a),
            I64Op::Modulo(a, b) | I64Op::Arithmetic(_, a, b) => covers(a) && covers(b),
        }
    }

    fn op<'a>(op: &I64Op, chunk: &Chunk<'a>, active: Active<'_>) -> Values<'a, i64> {
        let operands = |a, b| Ok::<_, Failed>((eval(a, chunk, active)?, eval(b, chunk, active)?));
        match op {
            I64Op::Negate(a) => unary(&eval(a, chunk, active)?, active, arithmetic::int_negate),
            I64Op::Abs(a) => unary(&eval(a, chunk, active)?, active, arithmetic::int_abs),
            I64Op::Modulo(a, b) => {
                let (a, b) = operands(a, b)?;
                binary(&a, &b, active, arithmetic::int_modulo)
            }
            I64Op::Arithmetic(op, a, b) => {
                let (a, b) = operands(a, b)?;
                match op {
                    Arithmetic::Add => {
                        binary(&a, &b, active, |a, b| arithmetic::int(Arithmetic::Add, a, b))
                    }
                    Arithmetic::Subtract => {
                        binary(&a, &b, active, |a, b| arithmetic::int(Arithmetic::Subtract, a, b))
                    }
                    Arithmetic::Multiply => {
                        binary(&a, &b, active, |a, b| arithmetic::int(Arithmetic::Multiply, a, b))
                    }
                    Arithmetic::Divide => {
                        binary(&a, &b, active, |a, b| arithmetic::int(Arithmetic::Divide, a, b))
                    }
                }
            }
        }
    }

    fn cast<'a>(from: &AnyExpr, chunk: &Chunk<'a>, active: Active<'_>) -> Values<'a, i64> {
        cast(from, chunk, active)
    }
}

/// `a op b` on floats, the operator fixed outside the loop.
fn float_arithmetic<'a, F: NotNull + SqlFloat>(
    op: Arithmetic,
    a: &Expr<F>,
    b: &Expr<F>,
    chunk: &Chunk<'a>,
    active: Active<'_>,
) -> Values<'a, F> {
    let (a, b) = (eval(a, chunk, active)?, eval(b, chunk, active)?);
    match op {
        Arithmetic::Add => binary(&a, &b, active, |a, b| arithmetic::float(Arithmetic::Add, a, b)),
        Arithmetic::Subtract => {
            binary(&a, &b, active, |a, b| arithmetic::float(Arithmetic::Subtract, a, b))
        }
        Arithmetic::Multiply => {
            binary(&a, &b, active, |a, b| arithmetic::float(Arithmetic::Multiply, a, b))
        }
        Arithmetic::Divide => {
            binary(&a, &b, active, |a, b| arithmetic::float(Arithmetic::Divide, a, b))
        }
    }
}

impl NotNull for f32 {
    fn covers_op(op: &F32Op) -> bool {
        match op {
            F32Op::Negate(a) | F32Op::Abs(a) => covers(a),
            F32Op::Arithmetic(_, a, b) => covers(a) && covers(b),
        }
    }

    fn op<'a>(op: &F32Op, chunk: &Chunk<'a>, active: Active<'_>) -> Values<'a, f32> {
        match op {
            F32Op::Negate(a) => unary(&eval(a, chunk, active)?, active, |a: f32| ok(-a)),
            F32Op::Abs(a) => unary(&eval(a, chunk, active)?, active, |a: f32| ok(a.abs())),
            F32Op::Arithmetic(op, a, b) => float_arithmetic(*op, a, b, chunk, active),
        }
    }

    fn cast<'a>(from: &AnyExpr, chunk: &Chunk<'a>, active: Active<'_>) -> Values<'a, f32> {
        cast(from, chunk, active)
    }
}

impl NotNull for f64 {
    fn covers_op(op: &F64Op) -> bool {
        match op {
            F64Op::Negate(a) | F64Op::Call(_, a) => covers(a),
            F64Op::Arithmetic(_, a, b) | F64Op::Power(a, b) => covers(a) && covers(b),
        }
    }

    fn op<'a>(op: &F64Op, chunk: &Chunk<'a>, active: Active<'_>) -> Values<'a, f64> {
        match op {
            F64Op::Negate(a) => unary(&eval(a, chunk, active)?, active, |a: f64| ok(-a)),
            F64Op::Arithmetic(op, a, b) => float_arithmetic(*op, a, b, chunk, active),
            F64Op::Call(function, a) => {
                unary(&eval(a, chunk, active)?, active, |a| arithmetic::call(*function, a))
            }
            F64Op::Power(a, b) => {
                let (a, b) = (eval(a, chunk, active)?, eval(b, chunk, active)?);
                binary(&a, &b, active, arithmetic::power)
            }
        }
    }

    fn cast<'a>(from: &AnyExpr, chunk: &Chunk<'a>, active: Active<'_>) -> Values<'a, f64> {
        cast(from, chunk, active)
    }
}
