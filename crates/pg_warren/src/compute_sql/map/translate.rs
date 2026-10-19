//! Translating an analysed `vector_map` expression into `warren-core`'s
//! typed IR: the compiled subset (DESIGN.md §6.3).
//!
//! Operators, functions and casts are recognised by the OID of the
//! `pg_catalog` function that implements them, so only Postgres's own
//! `float8`, `float4`, `int8` and `boolean` operations compile; a user's
//! operator or function, even one named `+`, does not. `integer` and
//! `smallint` values compile as `bigint` (every value, comparison and cast
//! of them is exact there), but their arithmetic, which overflows at a
//! smaller range, does not. A `numeric` constant compiles where it is cast
//! to `double precision` or `real`, by the same correctly rounded decimal
//! conversion Postgres makes; other `numeric` arithmetic does not. Anything
//! else is left to the executor, with the reason.

use std::ffi::CStr;

use pgrx::{pg_sys, PgList};
use warren_core::compute::{
    AnyExpr, Arithmetic, BoolOp, Comparands, Comparison, Expr, F32Op, F64Op, Function, I64Op,
};

use super::parse::{Analysed, Column};
use warren_core::compute::Dtype;

/// Why an expression is not in the compiled subset.
pub(crate) type Reason = String;

/// A compiled expression and the columns its operands are, or why the
/// expression is not in the compiled subset.
pub(crate) type Compiled = Result<(AnyExpr, Vec<usize>), Reason>;

/// An expression translated so far, by its SQL type.
#[derive(Debug, Clone)]
enum Typed {
    Bool(Expr<bool>),
    /// `integer` or `smallint`, held as `bigint`.
    Int4(Expr<i64>),
    I64(Expr<i64>),
    F32(Expr<f32>),
    F64(Expr<f64>),
}

impl Typed {
    fn any(self) -> AnyExpr {
        match self {
            Self::Bool(expr) => AnyExpr::Bool(expr),
            Self::Int4(expr) | Self::I64(expr) => AnyExpr::I64(expr),
            Self::F32(expr) => AnyExpr::F32(expr),
            Self::F64(expr) => AnyExpr::F64(expr),
        }
    }

    fn boolean(self) -> Result<Expr<bool>, Reason> {
        match self {
            Self::Bool(expr) => Ok(expr),
            other => Err(mismatch("boolean", &other)),
        }
    }

    /// A `bigint`, or an `integer` widened to one.
    fn int8(self) -> Result<Expr<i64>, Reason> {
        match self {
            Self::I64(expr) | Self::Int4(expr) => Ok(expr),
            other => Err(mismatch("bigint", &other)),
        }
    }

    fn int4(self) -> Result<Expr<i64>, Reason> {
        match self {
            Self::Int4(expr) => Ok(expr),
            other => Err(mismatch("integer", &other)),
        }
    }

    fn float4(self) -> Result<Expr<f32>, Reason> {
        match self {
            Self::F32(expr) => Ok(expr),
            other => Err(mismatch("real", &other)),
        }
    }

    fn float8(self) -> Result<Expr<f64>, Reason> {
        match self {
            Self::F64(expr) => Ok(expr),
            other => Err(mismatch("double precision", &other)),
        }
    }

    /// A `real` widened to `double precision` (as `float48` operators do).
    fn widened(self) -> Result<Expr<f64>, Reason> {
        match self {
            Self::F32(expr) => Ok(Expr::Cast(Box::new(AnyExpr::F32(expr)))),
            Self::F64(expr) => Ok(expr),
            other => Err(mismatch("double precision", &other)),
        }
    }

    /// The same kind of value as `self`, holding `expr`.
    fn tag(&self) -> &'static str {
        match self {
            Self::Bool(_) => "boolean",
            Self::Int4(_) => "integer",
            Self::I64(_) => "bigint",
            Self::F32(_) => "real",
            Self::F64(_) => "double precision",
        }
    }
}

fn mismatch(expected: &str, found: &Typed) -> Reason {
    format!("expected a {expected} operand, found {}", found.tag())
}

/// A function's name, for reasons.
fn function_name(oid: pg_sys::Oid) -> String {
    // SAFETY: get_func_name returns NULL or a palloc'd NUL-terminated name.
    let name = unsafe { pg_sys::get_func_name(oid) };
    if name.is_null() {
        return format!("function {}", u32::from(oid));
    }
    // SAFETY: checked non-NULL above.
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    format!("function {name}")
}

/// The translation of one expression tree.
struct Translator<'a> {
    columns: &'a [Column],
    /// The columns the IR's operands are, in operand order.
    operands: Vec<usize>,
    /// The value `CaseTestExpr` stands for inside a simple `CASE`.
    case_test: Option<Typed>,
}

/// Translates `analysed`; returns the IR and the columns its operands
/// are, or why it is not in the compiled subset.
pub(crate) fn translate(analysed: &Analysed) -> Compiled {
    let mut translator =
        Translator { columns: &analysed.columns, operands: Vec::new(), case_test: None };
    let expr = translator.node(analysed.expr)?.any();
    Ok((expr, translator.operands))
}

/// The nodes of a `List` of expressions.
///
/// # Safety
///
/// `list` is NIL or a valid `List` of nodes.
unsafe fn nodes(list: *mut pg_sys::List) -> Vec<*mut pg_sys::Node> {
    // SAFETY: the caller guarantees a valid list.
    let list = unsafe { PgList::<pg_sys::Node>::from_pg(list) };
    list.iter_ptr().collect()
}

impl Translator<'_> {
    fn node(&mut self, node: *mut pg_sys::Node) -> Result<Typed, Reason> {
        if node.is_null() {
            return Err("an empty expression".into());
        }
        // SAFETY: `node` is a valid, non-NULL expression node of the
        // analysed tree; each branch reads it as the type its tag names.
        unsafe {
            match (*node).type_ {
                pg_sys::NodeTag::T_Var => self.var(&*node.cast()),
                pg_sys::NodeTag::T_Const => constant(&*node.cast()),
                pg_sys::NodeTag::T_OpExpr => {
                    let op = &*node.cast::<pg_sys::OpExpr>();
                    let args = self.args(op.args)?;
                    call(op.opfuncid, args)
                }
                pg_sys::NodeTag::T_FuncExpr => {
                    let function = &*node.cast::<pg_sys::FuncExpr>();
                    if let Some(literal) = numeric_cast(function)? {
                        return Ok(literal);
                    }
                    let args = self.args(function.args)?;
                    call(function.funcid, args)
                }
                pg_sys::NodeTag::T_BoolExpr => self.boolean(&*node.cast()),
                pg_sys::NodeTag::T_CaseExpr => self.case(&*node.cast()),
                pg_sys::NodeTag::T_CaseTestExpr => {
                    self.case_test.clone().ok_or_else(|| "a CASE test outside CASE".into())
                }
                pg_sys::NodeTag::T_NullIfExpr => {
                    let op = &*node.cast::<pg_sys::NullIfExpr>();
                    let args = self.args(op.args)?;
                    let first = args.first().cloned().ok_or("NULLIF without arguments")?;
                    let equal = call(op.opfuncid, args)?.boolean()?;
                    case(vec![(equal, null_like(&first))], first)
                }
                pg_sys::NodeTag::T_CoalesceExpr => {
                    let coalesce = &*node.cast::<pg_sys::CoalesceExpr>();
                    let args = self.args(coalesce.args)?;
                    variadic(args, Expr::Coalesce, Expr::Coalesce, Expr::Coalesce, Expr::Coalesce)
                }
                pg_sys::NodeTag::T_MinMaxExpr => {
                    let minmax = &*node.cast::<pg_sys::MinMaxExpr>();
                    let args = self.args(minmax.args)?;
                    match minmax.op {
                        pg_sys::MinMaxOp::IS_GREATEST => variadic(
                            args,
                            Expr::Greatest,
                            Expr::Greatest,
                            Expr::Greatest,
                            Expr::Greatest,
                        ),
                        _ => variadic(args, Expr::Least, Expr::Least, Expr::Least, Expr::Least),
                    }
                }
                tag => Err(format!("{tag:?} is not in the compiled subset")),
            }
        }
    }

    /// The translated arguments of a `List`.
    fn args(&mut self, list: *mut pg_sys::List) -> Result<Vec<Typed>, Reason> {
        // SAFETY: argument lists of analysed expressions are Lists of nodes.
        unsafe { nodes(list) }.into_iter().map(|arg| self.node(arg)).collect()
    }

    fn var(&mut self, var: &pg_sys::Var) -> Result<Typed, Reason> {
        let column = usize::try_from(var.varattno - 1).map_err(|_| "a whole-row reference")?;
        if var.varno != 1 || var.varlevelsup != 0 || column >= self.columns.len() {
            return Err("a reference outside the vectors".into());
        }
        let operand = match self.operands.iter().position(|&c| c == column) {
            Some(operand) => operand,
            None => {
                self.operands.push(column);
                self.operands.len() - 1
            }
        };
        Ok(match self.columns[column].dtype {
            Dtype::F64 => Typed::F64(Expr::Vector(operand)),
            Dtype::F32 => Typed::F32(Expr::Vector(operand)),
            Dtype::I64 => Typed::I64(Expr::Vector(operand)),
            Dtype::Bool => Typed::Bool(Expr::Vector(operand)),
        })
    }

    fn boolean(&mut self, expr: &pg_sys::BoolExpr) -> Result<Typed, Reason> {
        let args =
            self.args(expr.args)?.into_iter().map(Typed::boolean).collect::<Result<Vec<_>, _>>()?;
        Ok(Typed::Bool(Expr::Op(match expr.boolop {
            pg_sys::BoolExprType::AND_EXPR => BoolOp::And(args),
            pg_sys::BoolExprType::OR_EXPR => BoolOp::Or(args),
            _ => BoolOp::Not(Box::new(args.into_iter().next().ok_or("NOT without an argument")?)),
        })))
    }

    fn case(&mut self, expr: &pg_sys::CaseExpr) -> Result<Typed, Reason> {
        let outer = self.case_test.take();
        if !expr.arg.is_null() {
            self.case_test = Some(self.node(expr.arg.cast())?);
        }
        let mut whens = Vec::new();
        // SAFETY: a CaseExpr's args are a List of CaseWhen nodes.
        for when in unsafe { nodes(expr.args) } {
            // SAFETY: as above.
            let when = unsafe { &*when.cast::<pg_sys::CaseWhen>() };
            let condition = self.node(when.expr.cast())?.boolean()?;
            whens.push((condition, self.node(when.result.cast())?));
        }
        self.case_test = outer;
        let otherwise = self.node(expr.defresult.cast())?;
        case(whens, otherwise)
    }
}

/// A typed NULL of `like`'s kind.
fn null_like(like: &Typed) -> Typed {
    match like {
        Typed::Bool(_) => Typed::Bool(Expr::Literal(None)),
        Typed::Int4(_) => Typed::Int4(Expr::Literal(None)),
        Typed::I64(_) => Typed::I64(Expr::Literal(None)),
        Typed::F32(_) => Typed::F32(Expr::Literal(None)),
        Typed::F64(_) => Typed::F64(Expr::Literal(None)),
    }
}

/// `CASE WHEN … THEN … ELSE otherwise END` over results of one kind.
fn case(whens: Vec<(Expr<bool>, Typed)>, otherwise: Typed) -> Result<Typed, Reason> {
    macro_rules! typed_case {
        ($variant:ident, $extract:ident, $otherwise:expr) => {{
            let when = whens
                .into_iter()
                .map(|(condition, value)| Ok((condition, value.$extract()?)))
                .collect::<Result<Vec<_>, Reason>>()?;
            Typed::$variant(Expr::Case { when, otherwise: Some(Box::new($otherwise)) })
        }};
    }
    Ok(match otherwise {
        Typed::Bool(otherwise) => typed_case!(Bool, boolean, otherwise),
        Typed::Int4(otherwise) => typed_case!(Int4, int4, otherwise),
        Typed::I64(otherwise) => typed_case!(I64, int8, otherwise),
        Typed::F32(otherwise) => typed_case!(F32, float4, otherwise),
        Typed::F64(otherwise) => typed_case!(F64, float8, otherwise),
    })
}

/// `COALESCE`, `LEAST` or `GREATEST` over arguments of one kind.
fn variadic(
    args: Vec<Typed>,
    bool_op: fn(Vec<Expr<bool>>) -> Expr<bool>,
    int_op: fn(Vec<Expr<i64>>) -> Expr<i64>,
    real_op: fn(Vec<Expr<f32>>) -> Expr<f32>,
    double_op: fn(Vec<Expr<f64>>) -> Expr<f64>,
) -> Result<Typed, Reason> {
    let first = args.first().ok_or("a call without arguments")?;
    macro_rules! all {
        ($extract:ident) => {
            args.into_iter().map(Typed::$extract).collect::<Result<Vec<_>, _>>()?
        };
    }
    Ok(match first {
        Typed::Bool(_) => Typed::Bool(bool_op(all!(boolean))),
        Typed::Int4(_) => Typed::Int4(int_op(all!(int4))),
        Typed::I64(_) => Typed::I64(int_op(all!(int8))),
        Typed::F32(_) => Typed::F32(real_op(all!(float4))),
        Typed::F64(_) => Typed::F64(double_op(all!(float8))),
    })
}

/// A constant of a type the IR holds.
fn constant(constant: &pg_sys::Const) -> Result<Typed, Reason> {
    let value = constant.constvalue.value();
    let null = constant.constisnull;
    fn pick<T>(null: bool, value: T) -> Option<T> {
        (!null).then_some(value)
    }
    Ok(match constant.consttype {
        pg_sys::FLOAT8OID => Typed::F64(Expr::Literal(pick(null, f64::from_bits(value as u64)))),
        pg_sys::FLOAT4OID => Typed::F32(Expr::Literal(pick(null, f32::from_bits(value as u32)))),
        pg_sys::INT8OID => Typed::I64(Expr::Literal(pick(null, value as i64))),
        pg_sys::INT4OID => Typed::Int4(Expr::Literal(pick(null, i64::from(value as i32)))),
        pg_sys::INT2OID => Typed::Int4(Expr::Literal(pick(null, i64::from(value as i16)))),
        pg_sys::BOOLOID => Typed::Bool(Expr::Literal(pick(null, value != 0))),
        other => Err(format!(
            "constants of type {} are not in the compiled subset",
            crate::source::type_name(other)
        ))?,
    })
}

/// A `numeric` constant cast to `double precision` or `real`, converted
/// as Postgres converts it (its decimal text, correctly rounded); `None`
/// if `function` is not such a cast.
fn numeric_cast(function: &pg_sys::FuncExpr) -> Result<Option<Typed>, Reason> {
    let to = match u32::from(function.funcid) {
        pg_sys::F_FLOAT8_NUMERIC => Dtype::F64,
        pg_sys::F_FLOAT4_NUMERIC => Dtype::F32,
        _ => return Ok(None),
    };
    // SAFETY: a FuncExpr's args are a List of nodes.
    let args = unsafe { nodes(function.args) };
    let [arg] = args.as_slice() else {
        return Ok(None);
    };
    // SAFETY: `arg` is a valid node.
    if !unsafe { pgrx::is_a(*arg, pg_sys::NodeTag::T_Const) } {
        return Ok(None);
    }
    // SAFETY: checked to be a Const above.
    let constant = unsafe { &*arg.cast::<pg_sys::Const>() };
    if constant.constisnull {
        return Ok(Some(match to {
            Dtype::F32 => Typed::F32(Expr::Literal(None)),
            _ => Typed::F64(Expr::Literal(None)),
        }));
    }
    let text = numeric_text(constant.constvalue);
    let out_of_range = || format!("numeric constant {text} is out of range for its cast");
    let zero = !text.chars().any(|c| c.is_ascii_digit() && c != '0');
    let infinite = text.contains("Infinity");
    Ok(Some(match to {
        Dtype::F32 => {
            let value: f32 = text.parse().map_err(|_| out_of_range())?;
            if (value == 0.0 && !zero) || (value.is_infinite() && !infinite) {
                return Err(out_of_range());
            }
            Typed::F32(Expr::Literal(Some(value)))
        }
        _ => {
            let value: f64 = text.parse().map_err(|_| out_of_range())?;
            if (value == 0.0 && !zero) || (value.is_infinite() && !infinite) {
                return Err(out_of_range());
            }
            Typed::F64(Expr::Literal(Some(value)))
        }
    }))
}

/// A `numeric` datum's text, by `numeric_out`.
fn numeric_text(datum: pg_sys::Datum) -> String {
    let (mut output, mut varlena) = (pg_sys::Oid::INVALID, false);
    // SAFETY: the out-pointers point to live locals; the datum is a
    // numeric, whose output function returns a palloc'd NUL-terminated
    // string.
    unsafe {
        pg_sys::getTypeOutputInfo(pg_sys::NUMERICOID, &mut output, &mut varlena);
        CStr::from_ptr(pg_sys::OidOutputFunctionCall(output, datum)).to_string_lossy().into_owned()
    }
}

/// `left op right` on floats or `bigint`.
fn float8_arithmetic(op: Arithmetic, mut args: Vec<Typed>, widen: bool) -> Result<Typed, Reason> {
    let (b, a) = (args.pop().ok_or("a missing operand")?, args.pop().ok_or("a missing operand")?);
    let (a, b) = match widen {
        true => (a.widened()?, b.widened()?),
        false => (a.float8()?, b.float8()?),
    };
    Ok(Typed::F64(Expr::Op(F64Op::Arithmetic(op, Box::new(a), Box::new(b)))))
}

/// The two arguments of a binary operation.
fn pair(mut args: Vec<Typed>) -> Result<(Typed, Typed), Reason> {
    let b = args.pop().ok_or("a missing operand")?;
    let a = args.pop().ok_or("a missing operand")?;
    Ok((a, b))
}

/// The one argument of a unary operation.
fn single(args: Vec<Typed>) -> Result<Typed, Reason> {
    args.into_iter().next().ok_or_else(|| "a missing operand".into())
}

/// How a comparison function compares.
#[derive(Clone, Copy)]
enum Compared {
    Float8,
    Float4,
    /// `float48` / `float84`: widened to `double precision`.
    Widened,
    /// `bigint`, possibly against `integer`.
    Int8,
    Int4,
    Bool,
}

/// The comparison a comparison function makes.
fn comparison(funcid: u32) -> Option<(Comparison, Compared)> {
    use pg_sys::*;
    use Compared::*;
    use Comparison::*;
    Some(match funcid {
        F_FLOAT8EQ => (Eq, Float8),
        F_FLOAT8NE => (Ne, Float8),
        F_FLOAT8LT => (Lt, Float8),
        F_FLOAT8LE => (Le, Float8),
        F_FLOAT8GT => (Gt, Float8),
        F_FLOAT8GE => (Ge, Float8),
        F_FLOAT4EQ => (Eq, Float4),
        F_FLOAT4NE => (Ne, Float4),
        F_FLOAT4LT => (Lt, Float4),
        F_FLOAT4LE => (Le, Float4),
        F_FLOAT4GT => (Gt, Float4),
        F_FLOAT4GE => (Ge, Float4),
        F_FLOAT48EQ | F_FLOAT84EQ => (Eq, Widened),
        F_FLOAT48NE | F_FLOAT84NE => (Ne, Widened),
        F_FLOAT48LT | F_FLOAT84LT => (Lt, Widened),
        F_FLOAT48LE | F_FLOAT84LE => (Le, Widened),
        F_FLOAT48GT | F_FLOAT84GT => (Gt, Widened),
        F_FLOAT48GE | F_FLOAT84GE => (Ge, Widened),
        F_INT8EQ | F_INT84EQ | F_INT48EQ => (Eq, Int8),
        F_INT8NE | F_INT84NE | F_INT48NE => (Ne, Int8),
        F_INT8LT | F_INT84LT | F_INT48LT => (Lt, Int8),
        F_INT8LE | F_INT84LE | F_INT48LE => (Le, Int8),
        F_INT8GT | F_INT84GT | F_INT48GT => (Gt, Int8),
        F_INT8GE | F_INT84GE | F_INT48GE => (Ge, Int8),
        F_INT4EQ => (Eq, Int4),
        F_INT4NE => (Ne, Int4),
        F_INT4LT => (Lt, Int4),
        F_INT4LE => (Le, Int4),
        F_INT4GT => (Gt, Int4),
        F_INT4GE => (Ge, Int4),
        F_BOOLEQ => (Eq, Bool),
        F_BOOLNE => (Ne, Bool),
        F_BOOLLT => (Lt, Bool),
        F_BOOLLE => (Le, Bool),
        F_BOOLGT => (Gt, Bool),
        F_BOOLGE => (Ge, Bool),
        _ => return None,
    })
}

/// The arithmetic operator and operand kind of an arithmetic function.
fn arithmetic(funcid: u32) -> Option<(Arithmetic, Compared)> {
    use pg_sys::*;
    use Arithmetic::*;
    use Compared::*;
    Some(match funcid {
        F_FLOAT8PL => (Add, Float8),
        F_FLOAT8MI => (Subtract, Float8),
        F_FLOAT8MUL => (Multiply, Float8),
        F_FLOAT8DIV => (Divide, Float8),
        F_FLOAT4PL => (Add, Float4),
        F_FLOAT4MI => (Subtract, Float4),
        F_FLOAT4MUL => (Multiply, Float4),
        F_FLOAT4DIV => (Divide, Float4),
        F_FLOAT48PL | F_FLOAT84PL => (Add, Widened),
        F_FLOAT48MI | F_FLOAT84MI => (Subtract, Widened),
        F_FLOAT48MUL | F_FLOAT84MUL => (Multiply, Widened),
        F_FLOAT48DIV | F_FLOAT84DIV => (Divide, Widened),
        F_INT8PL | F_INT84PL | F_INT48PL => (Add, Int8),
        F_INT8MI | F_INT84MI | F_INT48MI => (Subtract, Int8),
        F_INT8MUL | F_INT84MUL | F_INT48MUL => (Multiply, Int8),
        F_INT8DIV | F_INT84DIV | F_INT48DIV => (Divide, Int8),
        _ => return None,
    })
}

/// The whitelisted one-argument `double precision` function of an OID.
fn float8_function(funcid: u32) -> Option<Function> {
    use pg_sys::*;
    Some(match funcid {
        F_FLOAT8ABS | F_ABS_FLOAT8 => Function::Abs,
        F_DSQRT | F_SQRT_FLOAT8 => Function::Sqrt,
        F_DEXP | F_EXP_FLOAT8 => Function::Exp,
        F_DLOG1 | F_LN_FLOAT8 => Function::Ln,
        F_DLOG10 | F_LOG_FLOAT8 | F_LOG10_FLOAT8 => Function::Log,
        F_FLOOR_FLOAT8 => Function::Floor,
        F_CEIL_FLOAT8 | F_CEILING_FLOAT8 => Function::Ceil,
        F_DROUND | F_ROUND_FLOAT8 => Function::Round,
        F_SIGN_FLOAT8 => Function::Sign,
        _ => return None,
    })
}

/// A cast `from` one IR value to another.
fn cast(to: fn(Box<AnyExpr>) -> Typed, from: Typed) -> Typed {
    to(Box::new(from.any()))
}

/// The operator or function `funcid` applied to `args`.
fn call(funcid: pg_sys::Oid, args: Vec<Typed>) -> Result<Typed, Reason> {
    let id = u32::from(funcid);
    if let Some((comparison, kind)) = comparison(id) {
        let (a, b) = pair(args)?;
        let comparands = match kind {
            Compared::Float8 => Comparands::F64(a.float8()?, b.float8()?),
            Compared::Float4 => Comparands::F32(a.float4()?, b.float4()?),
            Compared::Widened => Comparands::F64(a.widened()?, b.widened()?),
            Compared::Int8 => Comparands::I64(a.int8()?, b.int8()?),
            Compared::Int4 => Comparands::I64(a.int4()?, b.int4()?),
            Compared::Bool => Comparands::Bool(a.boolean()?, b.boolean()?),
        };
        return Ok(Typed::Bool(Expr::Op(BoolOp::Compare(comparison, Box::new(comparands)))));
    }
    if let Some((op, kind)) = arithmetic(id) {
        return match kind {
            Compared::Float8 => float8_arithmetic(op, args, false),
            Compared::Widened => float8_arithmetic(op, args, true),
            Compared::Float4 => {
                let (a, b) = pair(args)?;
                let (a, b) = (Box::new(a.float4()?), Box::new(b.float4()?));
                Ok(Typed::F32(Expr::Op(F32Op::Arithmetic(op, a, b))))
            }
            _ => {
                let (a, b) = pair(args)?;
                let (a, b) = (Box::new(a.int8()?), Box::new(b.int8()?));
                Ok(Typed::I64(Expr::Op(I64Op::Arithmetic(op, a, b))))
            }
        };
    }
    if let Some(function) = float8_function(id) {
        let a = Box::new(single(args)?.float8()?);
        return Ok(Typed::F64(Expr::Op(F64Op::Call(function, a))));
    }
    Ok(match id {
        pg_sys::F_INT8MOD => {
            let (a, b) = pair(args)?;
            Typed::I64(Expr::Op(I64Op::Modulo(Box::new(a.int8()?), Box::new(b.int8()?))))
        }
        pg_sys::F_DPOW | pg_sys::F_POW_FLOAT8_FLOAT8 | pg_sys::F_POWER_FLOAT8_FLOAT8 => {
            let (a, b) = pair(args)?;
            Typed::F64(Expr::Op(F64Op::Power(Box::new(a.float8()?), Box::new(b.float8()?))))
        }
        pg_sys::F_FLOAT8UM => {
            Typed::F64(Expr::Op(F64Op::Negate(Box::new(single(args)?.float8()?))))
        }
        pg_sys::F_FLOAT4UM => {
            Typed::F32(Expr::Op(F32Op::Negate(Box::new(single(args)?.float4()?))))
        }
        pg_sys::F_INT8UM => Typed::I64(Expr::Op(I64Op::Negate(Box::new(single(args)?.int8()?)))),
        pg_sys::F_FLOAT4ABS | pg_sys::F_ABS_FLOAT4 => {
            Typed::F32(Expr::Op(F32Op::Abs(Box::new(single(args)?.float4()?))))
        }
        pg_sys::F_INT8ABS | pg_sys::F_ABS_INT8 => {
            Typed::I64(Expr::Op(I64Op::Abs(Box::new(single(args)?.int8()?))))
        }
        pg_sys::F_FLOAT8UP => Typed::F64(single(args)?.float8()?),
        pg_sys::F_FLOAT4UP => Typed::F32(single(args)?.float4()?),
        pg_sys::F_INT8UP => Typed::I64(single(args)?.int8()?),
        // Casts.
        pg_sys::F_FLOAT8_INT8
        | pg_sys::F_FLOAT8_INT4
        | pg_sys::F_FLOAT8_INT2
        | pg_sys::F_FLOAT8_FLOAT4 => cast(|from| Typed::F64(Expr::Cast(from)), single(args)?),
        pg_sys::F_FLOAT4_FLOAT8
        | pg_sys::F_FLOAT4_INT8
        | pg_sys::F_FLOAT4_INT4
        | pg_sys::F_FLOAT4_INT2 => cast(|from| Typed::F32(Expr::Cast(from)), single(args)?),
        pg_sys::F_INT8_FLOAT8 | pg_sys::F_INT8_FLOAT4 => {
            cast(|from| Typed::I64(Expr::Cast(from)), single(args)?)
        }
        pg_sys::F_INT8_INT4 | pg_sys::F_INT8_INT2 => Typed::I64(single(args)?.int8()?),
        pg_sys::F_INT4_INT2 => Typed::Int4(single(args)?.int4()?),
        pg_sys::F_BOOL_INT4 => {
            Typed::Bool(Expr::Cast(Box::new(AnyExpr::I64(single(args)?.int4()?))))
        }
        pg_sys::F_INT4_BOOL => {
            Typed::Int4(Expr::Cast(Box::new(AnyExpr::Bool(single(args)?.boolean()?))))
        }
        _ => return Err(format!("{} is not in the compiled subset", function_name(funcid))),
    })
}
