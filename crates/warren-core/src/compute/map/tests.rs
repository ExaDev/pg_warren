use std::sync::OnceLock;

use proptest::prelude::*;

use super::eval::Chunk;
use super::*;
use super::{arithmetic, eval, not_null};
use crate::compute::{ArithmeticError, Dtype, Element, Vector};
use crate::interrupts::{Cancelled, Uninterrupted};

fn workers() -> &'static Workers {
    static WORKERS: OnceLock<Workers> = OnceLock::new();
    WORKERS.get_or_init(|| Workers::new(3).unwrap())
}

fn memory() -> ComputeMemory {
    ComputeMemory::new(1 << 24)
}

fn vector<E: Element>(width: usize, values: Vec<E>) -> AnyVector
where
    AnyVector: From<Vector<E>>,
{
    let id_space = (values.len() / width) as u64;
    Vector::from_values(&memory(), id_space, width, values).unwrap().into()
}

fn run(id_space: u64, expr: &AnyExpr, operands: &[&AnyVector]) -> Result<AnyVector, ComputeError> {
    map(workers(), &memory(), &mut Uninterrupted, id_space, expr, operands)
}

fn f64s(result: Result<AnyVector, ComputeError>) -> Vec<f64> {
    f64::vector(&result.unwrap()).unwrap().values().to_vec()
}

// Expression builders, named after the SQL they stand for.
fn x(i: usize) -> Expr<f64> {
    Expr::Vector(i)
}
fn lit(v: f64) -> Expr<f64> {
    Expr::Literal(Some(v))
}
fn op(a: Expr<f64>, op: Arithmetic, b: Expr<f64>) -> Expr<f64> {
    Expr::Op(F64Op::Arithmetic(op, Box::new(a), Box::new(b)))
}
fn div(a: Expr<f64>, b: Expr<f64>) -> Expr<f64> {
    op(a, Arithmetic::Divide, b)
}
fn cmp(a: Expr<f64>, c: Comparison, b: Expr<f64>) -> Expr<bool> {
    Expr::Op(BoolOp::Compare(c, Box::new(Comparands::F64(a, b))))
}
fn call(f: Function, a: Expr<f64>) -> Expr<f64> {
    Expr::Op(F64Op::Call(f, Box::new(a)))
}

#[test]
fn nullif_guards_a_division_and_null_stores_nan() {
    // pr / NULLIF(deg, 0)
    let pr = vector(1, vec![1.0, 2.0, 3.0]);
    let deg = vector(1, vec![2.0, 0.0, 1.0]);
    let expr = AnyExpr::F64(div(x(0), Expr::NullIf(Box::new(x(1)), Box::new(lit(0.0)))));
    let found = f64s(run(3, &expr, &[&pr, &deg]));
    assert_eq!(found[0], 0.5);
    assert!(found[1].is_nan());
    assert_eq!(found[2], 3.0);
}

#[test]
fn case_with_or_yields_an_integer_vector() {
    // CASE WHEN is_account = 1 OR deg <= 50 THEN 1 ELSE 0 END
    let is_account = vector(1, vec![1.0, 0.0, 0.0]);
    let deg = vector(1, vec![99.0, 50.0, 51.0]);
    let condition = Expr::Op(BoolOp::Or(vec![
        cmp(x(0), Comparison::Eq, lit(1.0)),
        cmp(x(1), Comparison::Le, lit(50.0)),
    ]));
    let expr = AnyExpr::I64(Expr::Case {
        when: vec![(condition, Expr::Literal(Some(1)))],
        otherwise: Some(Box::new(Expr::Literal(Some(0)))),
    });
    let found = run(3, &expr, &[&is_account, &deg]).unwrap();
    assert_eq!(i64::vector(&found).unwrap().values(), [1, 1, 0]);
}

#[test]
fn scalars_and_width_1_vectors_broadcast_over_matrix_rows() {
    // 0.7 * emb + 0.3 * emb_nbr, then emb / deg
    let emb = vector(2, vec![1.0, 2.0, 3.0, 4.0]);
    let nbr = vector(2, vec![10.0, 20.0, 30.0, 40.0]);
    let deg = vector(1, vec![2.0, 4.0]);
    let smooth = op(
        op(lit(0.5), Arithmetic::Multiply, x(0)),
        Arithmetic::Add,
        op(lit(0.25), Arithmetic::Multiply, x(1)),
    );
    let found = run(2, &AnyExpr::F64(smooth), &[&emb, &nbr]).unwrap();
    assert_eq!(found.width(), 2);
    assert_eq!(f64::vector(&found).unwrap().values(), [3.0, 6.0, 9.0, 12.0]);
    let scaled = f64s(run(2, &AnyExpr::F64(div(x(0), x(1))), &[&emb, &deg]));
    assert_eq!(scaled, [0.5, 1.0, 0.75, 1.0]);
}

#[test]
fn mismatched_widths_are_refused() {
    let a = vector(2, vec![1.0; 4]);
    let b = vector(3, vec![1.0; 6]);
    let expr = AnyExpr::F64(op(x(0), Arithmetic::Add, x(1)));
    assert_eq!(
        run(2, &expr, &[&a, &b]).unwrap_err(),
        ComputeError::WidthMismatch { expected: 3, found: 2 }
    );
}

#[test]
fn operands_must_cover_the_id_space() {
    let a = vector(1, vec![1.0; 3]);
    assert_eq!(
        run(4, &AnyExpr::F64(x(0)), &[&a]).unwrap_err(),
        ComputeError::IdSpaceMismatch { expected: 4, found: 3 }
    );
}

#[test]
fn references_to_missing_or_mistyped_operands_are_errors_even_unevaluated() {
    let a = vector(1, Vec::<f64>::new());
    let unused = Expr::Case { when: vec![(Expr::Literal(Some(false)), x(1))], otherwise: None };
    assert_eq!(
        run(0, &AnyExpr::F64(unused), &[&a]).unwrap_err(),
        ComputeError::UnknownOperand { index: 1, operands: 1 }
    );
    let ints = vector(1, Vec::<i64>::new());
    assert_eq!(
        run(0, &AnyExpr::F64(x(0)), &[&ints]).unwrap_err(),
        ComputeError::OperandType { index: 0, expected: Dtype::F64, found: Dtype::I64 }
    );
}

#[test]
fn a_literal_expression_fills_the_id_space() {
    let found = run(3, &AnyExpr::Bool(Expr::Literal(Some(true))), &[]).unwrap();
    assert_eq!(bool::vector(&found).unwrap().values(), [true; 3]);
}

#[test]
fn guarded_branches_are_not_evaluated() {
    let zero = vector(1, vec![0.0, 2.0]);
    let inverse = || div(lit(1.0), x(0));
    let case = Expr::Case {
        when: vec![(cmp(x(0), Comparison::Eq, lit(0.0)), lit(0.0))],
        otherwise: Some(Box::new(inverse())),
    };
    assert_eq!(f64s(run(2, &AnyExpr::F64(case), &[&zero])), [0.0, 0.5]);
    let when = Expr::Case {
        when: vec![(cmp(x(0), Comparison::Ne, lit(0.0)), inverse())],
        otherwise: Some(Box::new(lit(-1.0))),
    };
    assert_eq!(f64s(run(2, &AnyExpr::F64(when), &[&zero])), [-1.0, 0.5]);
    let and = Expr::Op(BoolOp::And(vec![
        cmp(x(0), Comparison::Ne, lit(0.0)),
        cmp(inverse(), Comparison::Gt, lit(0.0)),
    ]));
    let found = run(2, &AnyExpr::Bool(and), &[&zero]).unwrap();
    assert_eq!(bool::vector(&found).unwrap().values(), [false, true]);
    let coalesce = Expr::Coalesce(vec![x(0), inverse()]);
    assert_eq!(f64s(run(2, &AnyExpr::F64(coalesce), &[&zero])), [0.0, 2.0]);
    let least = Expr::Least(vec![x(0), inverse()]);
    assert_eq!(
        run(2, &AnyExpr::F64(least), &[&zero]).unwrap_err(),
        ComputeError::Arithmetic(ArithmeticError::DivisionByZero)
    );
}

#[test]
fn the_first_failing_element_decides_the_error() {
    // sqrt(a) + 1 / b: a fails at element 4, b at element 2 (one chunk),
    // and at elements 8 and 6 (the next).
    let a = vector(1, vec![1.0, 1.0, 1.0, 1.0, -1.0, 1.0, 1.0, 1.0, -1.0, 1.0]);
    let b = vector(1, vec![1.0, 1.0, 0.0, 1.0, 1.0, 1.0, 0.0, 1.0, 1.0, 1.0]);
    let expr = AnyExpr::F64(op(call(Function::Sqrt, x(0)), Arithmetic::Add, div(lit(1.0), x(1))));
    assert_eq!(
        run(10, &expr, &[&a, &b]).unwrap_err(),
        ComputeError::Arithmetic(ArithmeticError::DivisionByZero)
    );
    let only_a = vector(1, vec![1.0; 10]);
    let expr = AnyExpr::F64(op(call(Function::Sqrt, x(0)), Arithmetic::Add, div(lit(1.0), x(1))));
    assert_eq!(
        run(10, &expr, &[&a, &only_a]).unwrap_err(),
        ComputeError::Arithmetic(ArithmeticError::SqrtOfNegative)
    );
}

#[test]
fn null_cannot_be_stored_in_integer_or_boolean_vectors() {
    let expr = AnyExpr::I64(Expr::Literal(None));
    assert_eq!(run(1, &expr, &[]).unwrap_err(), ComputeError::NullResult { dtype: Dtype::I64 });
    let expr = AnyExpr::Bool(Expr::Literal(None));
    assert_eq!(run(1, &expr, &[]).unwrap_err(), ComputeError::NullResult { dtype: Dtype::Bool });
    let found = run(1, &AnyExpr::F32(Expr::Literal(None)), &[]).unwrap();
    assert!(f32::vector(&found).unwrap().values()[0].is_nan());
}

#[test]
fn bigint_and_real_operations() {
    let a = vector(1, vec![7i64, -7, i64::MIN]);
    let b = vector(1, vec![2i64, 2, -1]);
    let int = |op: I64Op| run(3, &AnyExpr::I64(Expr::Op(op)), &[&a, &b]);
    let boxed = |i| Box::new(Expr::Vector(i));
    let ints = |result: Result<AnyVector, ComputeError>| {
        i64::vector(&result.unwrap()).unwrap().values().to_vec()
    };
    assert_eq!(ints(int(I64Op::Modulo(boxed(0), boxed(1)))), [1, -1, 0]);
    assert_eq!(
        int(I64Op::Arithmetic(Arithmetic::Divide, boxed(0), boxed(1))).unwrap_err(),
        ComputeError::Arithmetic(ArithmeticError::BigintOutOfRange)
    );
    assert_eq!(
        int(I64Op::Abs(boxed(0))).unwrap_err(),
        ComputeError::Arithmetic(ArithmeticError::BigintOutOfRange)
    );
    assert_eq!(ints(int(I64Op::Negate(boxed(1)))), [-2, -2, 1]);
    let real = vector(1, vec![1.5f32, -2.0, 0.1]);
    let reals = |op: F32Op| {
        run(3, &AnyExpr::F32(Expr::Op(op)), &[&real])
            .map(|v| f32::vector(&v).unwrap().values().to_vec())
    };
    let r = || Box::new(Expr::Vector(0));
    assert_eq!(reals(F32Op::Abs(r())), Ok(vec![1.5, 2.0, 0.1]));
    assert_eq!(reals(F32Op::Negate(r())), Ok(vec![-1.5, 2.0, -0.1]));
    assert_eq!(reals(F32Op::Arithmetic(Arithmetic::Add, r(), r())), Ok(vec![3.0, -4.0, 0.2]));
    // float4 + float8 is float8: the adapter casts the real operand.
    let widened = Expr::Cast(Box::new(AnyExpr::F32(Expr::Vector(0))));
    let found = f64s(run(3, &AnyExpr::F64(op(widened, Arithmetic::Add, lit(0.0))), &[&real]));
    assert_eq!(found[2], 0.10000000149011612);
}

#[test]
fn boolean_logic_is_three_valued() {
    let t = || Expr::Literal(Some(true));
    let f = || Expr::Literal(Some(false));
    let n = || Expr::<bool>::Literal(None);
    let eval = |e: Expr<bool>| {
        // Store through CASE so NULL is visible: NULL → 2.
        let expr = AnyExpr::I64(Expr::Case {
            when: vec![
                (e.clone(), Expr::Literal(Some(1))),
                (Expr::Op(BoolOp::Not(Box::new(e))), Expr::Literal(Some(0))),
            ],
            otherwise: Some(Box::new(Expr::Literal(Some(2)))),
        });
        i64::vector(&run(1, &expr, &[]).unwrap()).unwrap().values()[0]
    };
    let and = |a, b| Expr::Op(BoolOp::And(vec![a, b]));
    let or = |a, b| Expr::Op(BoolOp::Or(vec![a, b]));
    assert_eq!([eval(and(n(), f())), eval(and(n(), t())), eval(and(t(), t()))], [0, 2, 1]);
    assert_eq!([eval(or(n(), t())), eval(or(n(), f())), eval(or(f(), f()))], [1, 2, 0]);
    assert_eq!(eval(Expr::Op(BoolOp::Not(Box::new(n())))), 2);
    assert_eq!(eval(Expr::Op(BoolOp::And(vec![]))), 1);
    assert_eq!(eval(Expr::Op(BoolOp::Or(vec![]))), 0);
}

#[test]
fn least_and_greatest_skip_nulls_and_order_nan_last() {
    let values = |e: Expr<f64>| f64s(run(1, &AnyExpr::F64(e), &[]));
    let null = || Expr::Literal(None);
    assert_eq!(values(Expr::Least(vec![lit(f64::NAN), null(), lit(1.0)])), [1.0]);
    assert!(values(Expr::Greatest(vec![lit(1.0), lit(f64::NAN)]))[0].is_nan());
    assert!(values(Expr::Least(vec![lit(-0.0), lit(0.0)]))[0].is_sign_negative());
    assert!(values(Expr::Greatest(vec![null(), null()]))[0].is_nan());
    assert!(values(Expr::Coalesce(vec![]))[0].is_nan());
}

#[test]
fn an_interrupt_cancels_the_map() {
    struct Stop;
    impl Interrupts for Stop {
        fn check(&mut self) -> Result<(), Cancelled> {
            Err(Cancelled)
        }
    }
    let result = map(workers(), &memory(), &mut Stop, 3, &AnyExpr::F64(lit(1.0)), &[]);
    assert_eq!(result.unwrap_err(), ComputeError::Cancelled);
}

#[test]
fn the_result_is_charged_to_the_budget() {
    let tight = ComputeMemory::new(16);
    let result = map(workers(), &tight, &mut Uninterrupted, 3, &AnyExpr::F64(lit(1.0)), &[]);
    assert!(matches!(result, Err(ComputeError::MemoryLimit { .. })));
}

/// The naive reference: one element at a time, recursively, with SQL's
/// lazy CASE, COALESCE, AND and OR, over the `arithmetic` primitives.
mod reference {
    use super::*;

    pub(super) struct At<'a> {
        pub(super) operands: &'a [&'a AnyVector],
        pub(super) width: usize,
        pub(super) position: usize,
    }

    type Result<T> = std::result::Result<Option<T>, ArithmeticError>;

    impl At<'_> {
        fn load<T: Value>(&self, index: usize) -> Option<T> {
            let vector = T::vector(self.operands[index]).unwrap();
            let position = match vector.width() {
                1 => self.position / self.width,
                _ => self.position,
            };
            Some(vector.values()[position])
        }

        pub(super) fn f64(&self, e: &Expr<f64>) -> Result<f64> {
            self.common(
                e,
                |op| match op {
                    F64Op::Negate(a) => Ok(self.f64(a)?.map(|a| -a)),
                    F64Op::Arithmetic(o, a, b) => {
                        let (a, b) = (self.f64(a)?, self.f64(b)?);
                        a.zip(b).map(|(a, b)| arithmetic::float(*o, a, b)).transpose()
                    }
                    F64Op::Call(f, a) => self.f64(a)?.map(|a| arithmetic::call(*f, a)).transpose(),
                    F64Op::Power(a, b) => {
                        let (a, b) = (self.f64(a)?, self.f64(b)?);
                        a.zip(b).map(|(a, b)| arithmetic::power(a, b)).transpose()
                    }
                },
                |any| match any {
                    AnyExpr::I64(e) => Ok(self.i64(e)?.map(|v| v as f64)),
                    AnyExpr::F64(e) => self.f64(e),
                    _ => unreachable!("the generator casts only bigint"),
                },
            )
        }

        pub(super) fn i64(&self, e: &Expr<i64>) -> Result<i64> {
            self.common(
                e,
                |op| match op {
                    I64Op::Arithmetic(o, a, b) => {
                        let (a, b) = (self.i64(a)?, self.i64(b)?);
                        a.zip(b).map(|(a, b)| arithmetic::int(*o, a, b)).transpose()
                    }
                    I64Op::Modulo(a, b) => {
                        let (a, b) = (self.i64(a)?, self.i64(b)?);
                        a.zip(b).map(|(a, b)| arithmetic::int_modulo(a, b)).transpose()
                    }
                    I64Op::Negate(a) => self.i64(a)?.map(arithmetic::int_negate).transpose(),
                    I64Op::Abs(a) => self.i64(a)?.map(arithmetic::int_abs).transpose(),
                },
                |_| unreachable!("the generator casts nothing to bigint"),
            )
        }

        pub(super) fn bool(&self, e: &Expr<bool>) -> Result<bool> {
            self.common(
                e,
                |op| match op {
                    BoolOp::Compare(c, comparands) => {
                        let (a, b) = match comparands.as_ref() {
                            Comparands::F64(a, b) => (self.f64(a)?, self.f64(b)?),
                            _ => unreachable!("the generator compares double precision"),
                        };
                        Ok(a.zip(b).map(|(a, b)| {
                            let order = a.sql_cmp(b);
                            match c {
                                Comparison::Eq => order.is_eq(),
                                Comparison::Ne => order.is_ne(),
                                Comparison::Lt => order.is_lt(),
                                Comparison::Le => order.is_le(),
                                Comparison::Gt => order.is_gt(),
                                Comparison::Ge => order.is_ge(),
                            }
                        }))
                    }
                    BoolOp::Not(a) => Ok(self.bool(a)?.map(|a| !a)),
                    BoolOp::And(args) | BoolOp::Or(args) => {
                        let stop = matches!(op, BoolOp::Or(_));
                        let mut null = false;
                        for arg in args {
                            match self.bool(arg)? {
                                Some(v) if v == stop => return Ok(Some(stop)),
                                Some(_) => {}
                                None => null = true,
                            }
                        }
                        Ok((!null).then_some(!stop))
                    }
                },
                |_| unreachable!("the generator casts nothing to boolean"),
            )
        }

        fn common<T: Value>(
            &self,
            e: &Expr<T>,
            op: impl Fn(&T::Op) -> Result<T>,
            cast: impl Fn(&AnyExpr) -> Result<T>,
        ) -> Result<T>
        where
            Self: Evaluate<T>,
        {
            match e {
                Expr::Literal(v) => Ok(*v),
                Expr::Vector(i) => Ok(self.load(*i)),
                Expr::Cast(any) => cast(any),
                Expr::Case { when, otherwise } => {
                    for (condition, value) in when {
                        if self.bool(condition)? == Some(true) {
                            return self.evaluate(value);
                        }
                    }
                    otherwise.as_ref().map_or(Ok(None), |o| self.evaluate(o))
                }
                Expr::NullIf(a, b) => {
                    let (a, b) = (self.evaluate(a)?, self.evaluate(b)?);
                    Ok(match (a, b) {
                        (Some(x), Some(y)) if x.sql_cmp(y).is_eq() => None,
                        (a, _) => a,
                    })
                }
                Expr::Coalesce(args) => {
                    for arg in args {
                        if let Some(v) = self.evaluate(arg)? {
                            return Ok(Some(v));
                        }
                    }
                    Ok(None)
                }
                Expr::Least(args) | Expr::Greatest(args) => {
                    let want = if matches!(e, Expr::Least(_)) {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    };
                    let mut best: Option<T> = None;
                    let values: Vec<Option<T>> = args
                        .iter()
                        .map(|a| self.evaluate(a))
                        .collect::<std::result::Result<_, _>>()?;
                    for v in values.into_iter().flatten() {
                        if best.is_none_or(|b| v.sql_cmp(b) == want) {
                            best = Some(v);
                        }
                    }
                    Ok(best)
                }
                Expr::Op(o) => op(o),
            }
        }
    }

    pub(super) trait Evaluate<T> {
        fn evaluate(&self, e: &Expr<T>) -> Result<T>
        where
            T: Value;
    }

    impl Evaluate<f64> for At<'_> {
        fn evaluate(&self, e: &Expr<f64>) -> Result<f64> {
            self.f64(e)
        }
    }

    impl Evaluate<i64> for At<'_> {
        fn evaluate(&self, e: &Expr<i64>) -> Result<i64> {
            self.i64(e)
        }
    }

    impl Evaluate<bool> for At<'_> {
        fn evaluate(&self, e: &Expr<bool>) -> Result<bool> {
            self.bool(e)
        }
    }
}

fn special_f64() -> impl Strategy<Value = f64> {
    prop_oneof![
        6 => (-4i8..5).prop_map(f64::from),
        1 => Just(0.5),
        1 => Just(-0.0),
        1 => Just(f64::NAN),
        1 => Just(f64::INFINITY),
        1 => Just(1e300),
        1 => Just(1e-300),
    ]
}

fn double_leaf() -> impl Strategy<Value = Expr<f64>> {
    prop_oneof![
        3 => special_f64().prop_map(|v| Expr::Literal(Some(v))),
        1 => Just(Expr::Literal(None)),
        3 => (0usize..2).prop_map(Expr::Vector),
        1 => Just(Expr::Cast(Box::new(AnyExpr::I64(Expr::Vector(2))))),
    ]
}

fn comparisons() -> impl Strategy<Value = Comparison> {
    prop::sample::select(vec![
        Comparison::Eq,
        Comparison::Ne,
        Comparison::Lt,
        Comparison::Le,
        Comparison::Gt,
        Comparison::Ge,
    ])
}

fn condition(inner: BoxedStrategy<Expr<f64>>) -> impl Strategy<Value = Expr<bool>> {
    let compare = (comparisons(), inner.clone(), inner)
        .prop_map(|(c, a, b)| Expr::Op(BoolOp::Compare(c, Box::new(Comparands::F64(a, b)))));
    let leaf = prop_oneof![compare, prop::option::of(any::<bool>()).prop_map(Expr::Literal)];
    leaf.prop_recursive(2, 6, 3, |inner| {
        prop_oneof![
            inner.clone().prop_map(|a| Expr::Op(BoolOp::Not(Box::new(a)))),
            prop::collection::vec(inner.clone(), 0..3).prop_map(|a| Expr::Op(BoolOp::And(a))),
            prop::collection::vec(inner, 0..3).prop_map(|a| Expr::Op(BoolOp::Or(a))),
        ]
    })
}

fn functions() -> impl Strategy<Value = Function> + Clone {
    prop::sample::select(vec![
        Function::Abs,
        Function::Sqrt,
        Function::Exp,
        Function::Ln,
        Function::Log,
        Function::Floor,
        Function::Ceil,
        Function::Round,
        Function::Sign,
    ])
}

fn arithmetics() -> impl Strategy<Value = Arithmetic> + Clone {
    prop::sample::select(vec![
        Arithmetic::Add,
        Arithmetic::Subtract,
        Arithmetic::Multiply,
        Arithmetic::Divide,
    ])
}

fn doubles() -> impl Strategy<Value = Expr<f64>> {
    let (functions, arithmetic) = (functions(), arithmetics());
    double_leaf().prop_recursive(4, 24, 3, move |inner| {
        let b = || inner.clone().prop_map(Box::new);
        prop_oneof![
            b().prop_map(|a| Expr::Op(F64Op::Negate(a))),
            (arithmetic.clone(), b(), b())
                .prop_map(|(o, a, b)| Expr::Op(F64Op::Arithmetic(o, a, b))),
            (functions.clone(), b()).prop_map(|(f, a)| Expr::Op(F64Op::Call(f, a))),
            (b(), b()).prop_map(|(a, b)| Expr::Op(F64Op::Power(a, b))),
            (condition(inner.clone().boxed()), inner.clone(), prop::option::of(b()))
                .prop_map(|(c, v, otherwise)| Expr::Case { when: vec![(c, v)], otherwise }),
            (b(), b()).prop_map(|(a, b)| Expr::NullIf(a, b)),
            prop::collection::vec(inner.clone(), 0..3).prop_map(Expr::Coalesce),
            prop::collection::vec(inner.clone(), 0..3).prop_map(Expr::Least),
            prop::collection::vec(inner.clone(), 0..3).prop_map(Expr::Greatest),
        ]
    })
}

fn bigints() -> impl Strategy<Value = Expr<i64>> {
    let leaf = prop_oneof![
        prop_oneof![Just(0i64), Just(-1), Just(3), Just(i64::MIN), Just(i64::MAX)]
            .prop_map(|v| Expr::Literal(Some(v))),
        Just(Expr::Literal(None)),
        Just(Expr::Vector(2)),
    ];
    let arithmetic = arithmetics();
    leaf.prop_recursive(3, 12, 2, move |inner| {
        let b = || inner.clone().prop_map(Box::new);
        prop_oneof![
            b().prop_map(|a| Expr::Op(I64Op::Negate(a))),
            b().prop_map(|a| Expr::Op(I64Op::Abs(a))),
            (b(), b()).prop_map(|(a, b)| Expr::Op(I64Op::Modulo(a, b))),
            (arithmetic.clone(), b(), b())
                .prop_map(|(o, a, b)| Expr::Op(I64Op::Arithmetic(o, a, b))),
            prop::collection::vec(inner.clone(), 0..3).prop_map(Expr::Coalesce),
        ]
    })
}

/// A condition that is never NULL: comparisons, non-NULL literals and NOT.
fn not_null_condition(inner: BoxedStrategy<Expr<f64>>) -> impl Strategy<Value = Expr<bool>> {
    let compare = (comparisons(), inner.clone(), inner)
        .prop_map(|(c, a, b)| Expr::Op(BoolOp::Compare(c, Box::new(Comparands::F64(a, b)))));
    let leaf = prop_oneof![compare, any::<bool>().prop_map(|v| Expr::Literal(Some(v)))];
    leaf.prop_recursive(2, 4, 1, |inner| inner.prop_map(|a| Expr::Op(BoolOp::Not(Box::new(a)))))
}

/// `double precision` expressions with no NULL anywhere: the no-NULL
/// path's whole subset.
fn not_null_doubles() -> impl Strategy<Value = Expr<f64>> {
    let leaf = prop_oneof![
        3 => special_f64().prop_map(|v| Expr::Literal(Some(v))),
        3 => (0usize..2).prop_map(Expr::Vector),
        1 => Just(Expr::Cast(Box::new(AnyExpr::I64(Expr::Vector(2))))),
    ];
    let (functions, arithmetic) = (functions(), arithmetics());
    leaf.prop_recursive(4, 24, 3, move |inner| {
        let b = || inner.clone().prop_map(Box::new);
        prop_oneof![
            b().prop_map(|a| Expr::Op(F64Op::Negate(a))),
            (arithmetic.clone(), b(), b())
                .prop_map(|(o, a, b)| Expr::Op(F64Op::Arithmetic(o, a, b))),
            (functions.clone(), b()).prop_map(|(f, a)| Expr::Op(F64Op::Call(f, a))),
            (b(), b()).prop_map(|(a, b)| Expr::Op(F64Op::Power(a, b))),
            (not_null_condition(inner.clone().boxed()), inner.clone(), b()).prop_map(
                |(c, v, otherwise)| Expr::Case { when: vec![(c, v)], otherwise: Some(otherwise) }
            ),
        ]
    })
}

/// `bigint` expressions with no NULL anywhere.
fn not_null_bigints() -> impl Strategy<Value = Expr<i64>> {
    let leaf = prop_oneof![
        prop_oneof![Just(0i64), Just(-1), Just(3), Just(i64::MIN), Just(i64::MAX)]
            .prop_map(|v| Expr::Literal(Some(v))),
        Just(Expr::Vector(2)),
    ];
    let arithmetic = arithmetics();
    leaf.prop_recursive(3, 12, 2, move |inner| {
        let b = || inner.clone().prop_map(Box::new);
        prop_oneof![
            b().prop_map(|a| Expr::Op(I64Op::Negate(a))),
            b().prop_map(|a| Expr::Op(I64Op::Abs(a))),
            (b(), b()).prop_map(|(a, b)| Expr::Op(I64Op::Modulo(a, b))),
            (arithmetic.clone(), b(), b())
                .prop_map(|(o, a, b)| Expr::Op(I64Op::Arithmetic(o, a, b))),
        ]
    })
}

/// Random operands: a width-`d` and a width-1 `f64` vector and a width-1
/// `i64` vector over `n` rows.
fn operands() -> impl Strategy<Value = (usize, usize, Vec<f64>, Vec<f64>, Vec<i64>)> {
    (0usize..12, 1usize..4).prop_flat_map(|(n, d)| {
        (
            Just(n),
            Just(d),
            prop::collection::vec(special_f64(), n * d),
            prop::collection::vec(special_f64(), n),
            prop::collection::vec(prop_oneof![-3i64..4, Just(i64::MIN)], n),
        )
    })
}

/// The reference's result: every element, or the first failing element's
/// error.
fn expected<T: Value>(
    operands: &[&AnyVector],
    n: usize,
    d: usize,
    evaluate: impl Fn(&reference::At<'_>) -> Result<Option<T>, ArithmeticError>,
) -> Result<Vec<T>, ComputeError> {
    (0..n * d)
        .map(|position| {
            let element = reference::At { operands, width: d, position };
            match evaluate(&element)? {
                Some(v) => Ok(v),
                None => T::NULL.ok_or(ComputeError::NullResult { dtype: T::DTYPE }),
            }
        })
        .collect()
}

proptest! {
    #[test]
    fn double_expressions_match_a_row_at_a_time_reference(
        (n, d, a, b, c) in operands(), expr in prop_oneof![doubles(), not_null_doubles()]
    ) {
        let (a, b, c) = (vector(d, a), vector(1, b), vector(1, c));
        let operands = [&a, &b, &c];
        let want = expected(&operands, n, d, |e| e.f64(&expr));
        let found = run(n as u64, &AnyExpr::F64(expr.clone()), &operands)
            .map(|v| f64::vector(&v).unwrap().values().to_vec());
        match (&found, &want) {
            (Ok(found), Ok(want)) => prop_assert!(
                found.iter().zip(want).all(|(f, w)| f.to_bits() == w.to_bits() || (f.is_nan() && w.is_nan())),
                "found {:?}, want {:?}", found, want
            ),
            _ => prop_assert_eq!(found, want),
        }
    }

    #[test]
    fn bigint_expressions_match_a_row_at_a_time_reference(
        (n, d, a, b, c) in operands(), expr in prop_oneof![bigints(), not_null_bigints()]
    ) {
        let (a, b, c) = (vector(d, a), vector(1, b), vector(1, c));
        let operands = [&a, &b, &c];
        let want = expected(&operands, n, d, |e| e.i64(&expr));
        let found = run(n as u64, &AnyExpr::I64(expr.clone()), &[&a, &b, &c][..])
            .map(|v| i64::vector(&v).unwrap().values().to_vec());
        prop_assert_eq!(found, want);
    }
}

#[test]
fn pagerank_s_maps_take_the_no_null_path() {
    // CASE WHEN out > 0 THEN x / out ELSE 0 END, and the update
    // damping * (y + dangling * p) + teleport * p.
    let message = Expr::Case {
        when: vec![(cmp(x(1), Comparison::Gt, lit(0.0)), div(x(0), x(1)))],
        otherwise: Some(Box::new(lit(0.0))),
    };
    assert!(not_null::covers(&message));
    let damped = op(x(0), Arithmetic::Multiply, op(x(1), Arithmetic::Add, x(2)));
    assert!(not_null::covers(&op(damped, Arithmetic::Add, x(3))));
    assert!(not_null::covers(&Expr::<f64>::Cast(Box::new(AnyExpr::F32(Expr::Vector(0))))));
}

#[test]
fn expressions_that_can_be_null_take_the_columnar_path() {
    let null = || Expr::<f64>::Literal(None);
    let positive = || cmp(x(0), Comparison::Gt, lit(0.0));
    let no_else = Expr::Case { when: vec![(positive(), x(0))], otherwise: None };
    let and = Expr::Op(BoolOp::And(vec![positive()]));
    let unknown = Expr::Case { when: vec![(and, x(0))], otherwise: Some(Box::new(x(0))) };
    for expr in [
        null(),
        op(x(0), Arithmetic::Add, null()),
        Expr::NullIf(Box::new(x(0)), Box::new(lit(0.0))),
        Expr::Coalesce(vec![x(0)]),
        Expr::Least(vec![x(0)]),
        no_else,
        unknown,
    ] {
        assert!(!not_null::covers(&expr), "{expr:?}");
    }
}

/// Whether each element of a `len`-element chunk is active: all of them
/// unless `masked`, else as `mask` says, cycled.
fn activity(len: usize, mask: &[bool], masked: bool) -> Vec<bool> {
    (0..len).map(|i| !masked || mask[i % mask.len()]).collect()
}

/// The no-NULL path and the columnar path agree on `expr` over `chunk`:
/// both fail, or both succeed with the same bits at every active element,
/// where the columnar path found no NULL.
fn agree<T: not_null::NotNull, B: PartialEq + std::fmt::Debug>(
    expr: &Expr<T>,
    chunk: &Chunk<'_>,
    active: &[bool],
    masked: bool,
    bits: impl Fn(T) -> B,
) -> Result<(), TestCaseError> {
    if !not_null::covers(expr) {
        return Ok(());
    }
    let columnar = eval::eval(expr, chunk, active);
    let fast = not_null::eval(expr, chunk, masked.then_some(active));
    match (columnar, fast) {
        (Ok(columnar), Ok(fast)) => {
            prop_assert_eq!(fast.len(), chunk.len);
            for (i, _) in active.iter().enumerate().filter(|(_, &active)| active) {
                prop_assert!(!columnar.nulls[i], "NULL at {} of {:?}", i, expr);
                let (want, found) = (bits(columnar.values[i]), bits(fast[i]));
                prop_assert_eq!(want, found, "at {} of {:?}", i, expr);
            }
        }
        (Err(_), Err(_)) => {}
        (columnar, fast) => prop_assert!(
            false,
            "columnar {:?}, no-NULL path failed: {}",
            columnar.map(|c| c.values),
            fast.is_err()
        ),
    }
    Ok(())
}

proptest! {
    #[test]
    fn the_no_null_path_matches_the_columnar_path_on_doubles(
        (n, d, a, b, c) in operands(), expr in prop_oneof![doubles(), not_null_doubles()],
        (start, len) in (any::<prop::sample::Index>(), any::<prop::sample::Index>()),
        mask in prop::collection::vec(any::<bool>(), 1..8), masked in any::<bool>()
    ) {
        let (a, b, c) = (vector(d, a), vector(1, b), vector(1, c));
        let operands = [&a, &b, &c];
        let start = start.index(n * d + 1);
        let len = len.index(n * d - start + 1);
        let chunk = Chunk { operands: &operands, width: d, start, len };
        let active = activity(len, &mask, masked);
        agree(&expr, &chunk, &active, masked, f64::to_bits)?;
    }

    #[test]
    fn the_no_null_path_matches_the_columnar_path_on_bigints(
        (n, d, a, b, c) in operands(), expr in prop_oneof![bigints(), not_null_bigints()],
        (start, len) in (any::<prop::sample::Index>(), any::<prop::sample::Index>()),
        mask in prop::collection::vec(any::<bool>(), 1..8), masked in any::<bool>()
    ) {
        let (a, b, c) = (vector(d, a), vector(1, b), vector(1, c));
        let operands = [&a, &b, &c];
        let start = start.index(n * d + 1);
        let len = len.index(n * d - start + 1);
        let chunk = Chunk { operands: &operands, width: d, start, len };
        let active = activity(len, &mask, masked);
        agree(&expr, &chunk, &active, masked, |v: i64| v)?;
    }
}
