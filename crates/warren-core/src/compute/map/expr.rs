//! The `vector_map` expression IR: the compiled subset of SQL expressions
//! (DESIGN.md §6.3), typed by construction.
//!
//! The adapter translates Postgres's analysed expression tree into this IR,
//! inserting each implicit cast Postgres's parse analysis inserted (for
//! example `float4 + float8` is a cast of the `float4` operand), so every
//! operator here takes operands of one type. Vector references name an
//! operand by its position in the operand list `map` is given.

use crate::compute::Element;

/// A typed SQL expression with values of type `T`.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr<T: Value> {
    /// A constant; `None` is a typed NULL.
    Literal(Option<T>),
    /// The value of operand vector `index` at the current element.
    Vector(usize),
    /// A cast from another type.
    Cast(Box<AnyExpr>),
    /// `CASE WHEN c₁ THEN v₁ … ELSE otherwise END`; without `ELSE`, NULL.
    Case {
        /// The `WHEN` conditions with their results, tried in order.
        when: Vec<(Expr<bool>, Expr<T>)>,
        /// The `ELSE` result.
        otherwise: Option<Box<Expr<T>>>,
    },
    /// `NULLIF(a, b)`: NULL where `a = b`, else `a`.
    NullIf(Box<Expr<T>>, Box<Expr<T>>),
    /// `COALESCE(…)`: the first argument that is not NULL.
    Coalesce(Vec<Expr<T>>),
    /// `LEAST(…)`: the smallest argument that is not NULL.
    Least(Vec<Expr<T>>),
    /// `GREATEST(…)`: the largest argument that is not NULL.
    Greatest(Vec<Expr<T>>),
    /// An operation particular to `T`.
    Op(T::Op),
}

/// An expression of any element type.
#[derive(Debug, Clone, PartialEq)]
pub enum AnyExpr {
    /// A `boolean` expression.
    Bool(Expr<bool>),
    /// A `bigint` expression.
    I64(Expr<i64>),
    /// A `real` expression.
    F32(Expr<f32>),
    /// A `double precision` expression.
    F64(Expr<f64>),
}

/// A value type of the IR, with the operations particular to it.
pub trait Value: Element {
    /// The operations whose result has this type.
    type Op: std::fmt::Debug + Clone + PartialEq + Send + Sync;

    /// What a vector of this type stores where an expression is NULL:
    /// `NaN` for floats; `None` for `i64` and `bool`, which have no spare
    /// value, so storing NULL there is an error.
    const NULL: Option<Self>;
}

impl Value for bool {
    type Op = BoolOp;
    const NULL: Option<Self> = None;
}

impl Value for i64 {
    type Op = I64Op;
    const NULL: Option<Self> = None;
}

impl Value for f32 {
    type Op = F32Op;
    const NULL: Option<Self> = Some(f32::NAN);
}

impl Value for f64 {
    type Op = F64Op;
    const NULL: Option<Self> = Some(f64::NAN);
}

/// `+ − × ÷` on numbers of one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arithmetic {
    /// `a + b`.
    Add,
    /// `a - b`.
    Subtract,
    /// `a * b`.
    Multiply,
    /// `a / b`; truncating for `bigint`.
    Divide,
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Comparison {
    /// `=`.
    Eq,
    /// `<>`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
}

/// The two operands of a comparison, of one type.
#[derive(Debug, Clone, PartialEq)]
pub enum Comparands {
    /// `boolean` operands.
    Bool(Expr<bool>, Expr<bool>),
    /// `bigint` operands.
    I64(Expr<i64>, Expr<i64>),
    /// `real` operands.
    F32(Expr<f32>, Expr<f32>),
    /// `double precision` operands.
    F64(Expr<f64>, Expr<f64>),
}

/// The operations with a `boolean` result.
#[derive(Debug, Clone, PartialEq)]
pub enum BoolOp {
    /// A comparison in SQL order (`NaN` equals `NaN` and sorts last).
    Compare(Comparison, Box<Comparands>),
    /// `NOT a`.
    Not(Box<Expr<bool>>),
    /// `a AND b AND …`, evaluated left to right, stopping at false.
    And(Vec<Expr<bool>>),
    /// `a OR b OR …`, evaluated left to right, stopping at true.
    Or(Vec<Expr<bool>>),
}

/// The operations with a `bigint` result.
#[derive(Debug, Clone, PartialEq)]
pub enum I64Op {
    /// `-a`.
    Negate(Box<Expr<i64>>),
    /// `a op b`.
    Arithmetic(Arithmetic, Box<Expr<i64>>, Box<Expr<i64>>),
    /// `a % b`.
    Modulo(Box<Expr<i64>>, Box<Expr<i64>>),
    /// `abs(a)`.
    Abs(Box<Expr<i64>>),
}

/// The operations with a `real` result.
#[derive(Debug, Clone, PartialEq)]
pub enum F32Op {
    /// `-a`.
    Negate(Box<Expr<f32>>),
    /// `a op b`.
    Arithmetic(Arithmetic, Box<Expr<f32>>, Box<Expr<f32>>),
    /// `abs(a)`.
    Abs(Box<Expr<f32>>),
}

/// The operations with a `double precision` result.
#[derive(Debug, Clone, PartialEq)]
pub enum F64Op {
    /// `-a`.
    Negate(Box<Expr<f64>>),
    /// `a op b`.
    Arithmetic(Arithmetic, Box<Expr<f64>>, Box<Expr<f64>>),
    /// `function(a)`.
    Call(Function, Box<Expr<f64>>),
    /// `power(a, b)`, also written `a ^ b`.
    Power(Box<Expr<f64>>, Box<Expr<f64>>),
}

/// The whitelisted one-argument `double precision` functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Function {
    /// `abs`.
    Abs,
    /// `sqrt`.
    Sqrt,
    /// `exp`.
    Exp,
    /// `ln`: the natural logarithm.
    Ln,
    /// `log`: the base-10 logarithm.
    Log,
    /// `floor`.
    Floor,
    /// `ceil`.
    Ceil,
    /// `round`: half to even, as `rint`.
    Round,
    /// `sign`: −1, 0 or 1 (0 for `NaN`).
    Sign,
}
