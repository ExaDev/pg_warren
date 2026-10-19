//! SQL arithmetic on single values, exactly as Postgres's `float.c` and
//! `int8.c` define it, so the compiled path and the executor agree bit for
//! bit and raise the same errors.

use std::ops::{Add, Div, Mul, Neg, Sub};

use super::{Arithmetic, Function};
use crate::compute::ArithmeticError;

/// `f32` or `f64`, as Postgres's `float4` and `float8` operators treat
/// them.
pub(super) trait SqlFloat:
    Copy
    + PartialOrd
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<Output = Self>
    + Div<Output = Self>
    + Neg<Output = Self>
{
    const ZERO: Self;

    fn is_nan(self) -> bool;

    fn is_infinite(self) -> bool;
}

macro_rules! sql_float {
    ($type:ty) => {
        impl SqlFloat for $type {
            const ZERO: Self = 0.0;

            fn is_nan(self) -> bool {
                <$type>::is_nan(self)
            }

            fn is_infinite(self) -> bool {
                <$type>::is_infinite(self)
            }
        }
    };
}

sql_float!(f32);
sql_float!(f64);

/// `a op b` on floats (`float8_pl`, `float8_mi`, `float8_mul`,
/// `float8_div` and their `float4` twins).
pub(super) fn float<F: SqlFloat>(op: Arithmetic, a: F, b: F) -> Result<F, ArithmeticError> {
    if op == Arithmetic::Divide && b == F::ZERO && !a.is_nan() {
        return Err(ArithmeticError::DivisionByZero);
    }
    let result = match op {
        Arithmetic::Add => a + b,
        Arithmetic::Subtract => a - b,
        Arithmetic::Multiply => a * b,
        Arithmetic::Divide => a / b,
    };
    let finite_operands = match op {
        Arithmetic::Divide => !a.is_infinite(),
        _ => !a.is_infinite() && !b.is_infinite(),
    };
    if result.is_infinite() && finite_operands {
        return Err(ArithmeticError::FloatOverflow);
    }
    let underflowed = match op {
        Arithmetic::Multiply => a != F::ZERO && b != F::ZERO,
        Arithmetic::Divide => a != F::ZERO && !b.is_infinite(),
        Arithmetic::Add | Arithmetic::Subtract => false,
    };
    if result == F::ZERO && underflowed {
        return Err(ArithmeticError::FloatUnderflow);
    }
    Ok(result)
}

/// `a op b` on `bigint` (`int8pl` and friends): overflow and division by
/// zero are errors.
pub(super) fn int(op: Arithmetic, a: i64, b: i64) -> Result<i64, ArithmeticError> {
    if op == Arithmetic::Divide && b == 0 {
        return Err(ArithmeticError::DivisionByZero);
    }
    let result = match op {
        Arithmetic::Add => a.checked_add(b),
        Arithmetic::Subtract => a.checked_sub(b),
        Arithmetic::Multiply => a.checked_mul(b),
        Arithmetic::Divide => a.checked_div(b),
    };
    result.ok_or(ArithmeticError::BigintOutOfRange)
}

/// `a % b` on `bigint` (`int8mod`): the sign of `a`; `MIN % -1` is 0.
pub(super) fn int_modulo(a: i64, b: i64) -> Result<i64, ArithmeticError> {
    match b {
        0 => Err(ArithmeticError::DivisionByZero),
        -1 => Ok(0),
        b => Ok(a % b),
    }
}

/// `-a` on `bigint` (`int8um`).
pub(super) fn int_negate(a: i64) -> Result<i64, ArithmeticError> {
    a.checked_neg().ok_or(ArithmeticError::BigintOutOfRange)
}

/// `abs(a)` on `bigint` (`int8abs`).
pub(super) fn int_abs(a: i64) -> Result<i64, ArithmeticError> {
    a.checked_abs().ok_or(ArithmeticError::BigintOutOfRange)
}

/// `function(a)` on `double precision` (`dexp`, `dlog1`, `dlog10`,
/// `dsqrt`, `dfloor`, `dceil`, `dround`, `dsign`, `float8abs`).
pub(super) fn call(function: Function, a: f64) -> Result<f64, ArithmeticError> {
    Ok(match function {
        Function::Abs => a.abs(),
        Function::Floor => a.floor(),
        Function::Ceil => a.ceil(),
        Function::Round => a.round_ties_even(),
        Function::Sign if a > 0.0 => 1.0,
        Function::Sign if a < 0.0 => -1.0,
        Function::Sign => 0.0,
        Function::Exp => return exp(a),
        Function::Sqrt => {
            if a < 0.0 {
                return Err(ArithmeticError::SqrtOfNegative);
            }
            a.sqrt()
        }
        Function::Ln | Function::Log => {
            if a == 0.0 {
                return Err(ArithmeticError::LogOfZero);
            }
            if a < 0.0 {
                return Err(ArithmeticError::LogOfNegative);
            }
            if function == Function::Ln {
                a.ln()
            } else {
                a.log10()
            }
        }
    })
}

/// `exp(a)` (`dexp`): infinities and NaN by hand, and a result that
/// overflows or underflows to zero is an error.
fn exp(a: f64) -> Result<f64, ArithmeticError> {
    if a.is_nan() {
        return Ok(a);
    }
    if a.is_infinite() {
        return Ok(if a > 0.0 { a } else { 0.0 });
    }
    checked(a.exp(), a)
}

/// A finite `pow` or `exp` result: infinite is an overflow, and zero from
/// a non-zero base an underflow.
fn checked(result: f64, base: f64) -> Result<f64, ArithmeticError> {
    if result.is_infinite() {
        return Err(ArithmeticError::FloatOverflow);
    }
    if result == 0.0 && base != 0.0 {
        return Err(ArithmeticError::FloatUnderflow);
    }
    Ok(result)
}

/// `power(a, b)` (`dpow`): POSIX results for NaN and infinities computed by
/// hand, SQL errors for the undefined and complex cases.
pub(super) fn power(a: f64, b: f64) -> Result<f64, ArithmeticError> {
    if a.is_nan() {
        return Ok(if b.is_nan() || b != 0.0 { f64::NAN } else { 1.0 });
    }
    if b.is_nan() {
        return Ok(if a != 1.0 { f64::NAN } else { 1.0 });
    }
    if a == 0.0 && b < 0.0 {
        return Err(ArithmeticError::ZeroToNegativePower);
    }
    if a < 0.0 && b.floor() != b {
        return Err(ArithmeticError::ComplexPower);
    }
    if b.is_infinite() {
        let magnitude = a.abs();
        return Ok(match (magnitude == 1.0, b > 0.0, magnitude > 1.0) {
            (true, _, _) => 1.0,
            (false, true, true) | (false, false, false) => f64::INFINITY,
            (false, true, false) | (false, false, true) => 0.0,
        });
    }
    if a.is_infinite() {
        if b == 0.0 {
            return Ok(1.0);
        }
        if a > 0.0 {
            return Ok(if b > 0.0 { a } else { 0.0 });
        }
        // a = −∞; b is an integer, odd when b / 2 is not.
        let half = b / 2.0;
        let odd = half.floor() != half;
        return Ok(match (b > 0.0, odd) {
            (true, true) => a,
            (true, false) => -a,
            (false, true) => -0.0,
            (false, false) => 0.0,
        });
    }
    checked(a.powf(b), a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute::ArithmeticError::*;
    use Arithmetic::*;

    const NAN: f64 = f64::NAN;
    const INF: f64 = f64::INFINITY;

    /// Equal as SQL text would show them: NaN matches NaN, and the sign of
    /// zero matters.
    fn same(found: Result<f64, ArithmeticError>, expected: Result<f64, ArithmeticError>) {
        match (found, expected) {
            (Ok(f), Ok(e)) => assert!(
                (f.is_nan() && e.is_nan()) || f.to_bits() == e.to_bits(),
                "found {f:e}, expected {e:e}"
            ),
            (found, expected) => assert_eq!(found, expected),
        }
    }

    #[test]
    fn float_operators_check_like_float_c() {
        same(float(Divide, NAN, 0.0), Ok(NAN));
        same(float(Divide, 1.0, 0.0), Err(DivisionByZero));
        same(float(Divide, 0.0, -0.0), Err(DivisionByZero));
        same(float(Divide, INF, 0.0), Err(DivisionByZero));
        same(float(Multiply, 1e308, 10.0), Err(FloatOverflow));
        same(float(Multiply, 1e-308, 1e-10), Ok(1e-318));
        same(float(Multiply, 1e-308, 1e-100), Err(FloatUnderflow));
        same(float(Multiply, INF, 0.0), Ok(NAN));
        same(float(Subtract, INF, INF), Ok(NAN));
        same(float(Add, 1e308, 1e308), Err(FloatOverflow));
        same(float(Add, INF, 1.0), Ok(INF));
        same(float(Divide, 1.0, 1e-310), Err(FloatOverflow));
        same(float(Divide, 1e-320, 1e10), Err(FloatUnderflow));
        same(float(Divide, 1.0, INF), Ok(0.0));
        same(float(Divide, INF, INF), Ok(NAN));
        same(float(Multiply, -0.0, 1.0), Ok(-0.0));
        assert_eq!(float(Multiply, 1e38f32, 10.0), Err(FloatOverflow));
        assert!(float(Divide, f32::NAN, 0.0).unwrap().is_nan());
        assert_eq!(float(Add, 0.1f32, 0.2), Ok(0.1f32 + 0.2f32));
    }

    #[test]
    fn bigint_operators_check_like_int8_c() {
        assert_eq!(int(Add, i64::MAX, 1), Err(BigintOutOfRange));
        assert_eq!(int(Subtract, i64::MIN, 1), Err(BigintOutOfRange));
        assert_eq!(int(Multiply, i64::MAX, 2), Err(BigintOutOfRange));
        assert_eq!(int(Divide, -7, 2), Ok(-3));
        assert_eq!(int(Divide, 1, 0), Err(DivisionByZero));
        assert_eq!(int(Divide, i64::MIN, -1), Err(BigintOutOfRange));
        assert_eq!(int_modulo(7, -3), Ok(1));
        assert_eq!(int_modulo(-7, 3), Ok(-1));
        assert_eq!(int_modulo(i64::MIN, -1), Ok(0));
        assert_eq!(int_modulo(5, 0), Err(DivisionByZero));
        assert_eq!(int_negate(i64::MIN), Err(BigintOutOfRange));
        assert_eq!(int_negate(5), Ok(-5));
        assert_eq!(int_abs(i64::MIN), Err(BigintOutOfRange));
        assert_eq!(int_abs(-5), Ok(5));
    }

    #[test]
    fn functions_check_like_float_c() {
        use Function::*;
        same(call(Exp, -1000.0), Err(FloatUnderflow));
        same(call(Exp, 1000.0), Err(FloatOverflow));
        same(call(Exp, -708.0), Ok((-708f64).exp()));
        same(call(Exp, -740.0), Ok((-740f64).exp()));
        same(call(Exp, -INF), Ok(0.0));
        same(call(Exp, NAN), Ok(NAN));
        same(call(Ln, 0.0), Err(LogOfZero));
        same(call(Ln, -1.0), Err(LogOfNegative));
        same(call(Ln, INF), Ok(INF));
        same(call(Ln, 1.0), Ok(0.0));
        same(call(Ln, NAN), Ok(NAN));
        same(call(Log, 100.0), Ok(2.0));
        same(call(Log, 0.0), Err(LogOfZero));
        same(call(Log, 1e-320), Ok(1e-320f64.log10()));
        same(call(Sqrt, -1.0), Err(SqrtOfNegative));
        same(call(Sqrt, -0.0), Ok(-0.0));
        same(call(Sqrt, INF), Ok(INF));
        same(call(Round, 2.5), Ok(2.0));
        same(call(Round, -2.5), Ok(-2.0));
        same(call(Round, -0.4), Ok(-0.0));
        same(call(Floor, -0.5), Ok(-1.0));
        same(call(Ceil, -0.5), Ok(-0.0));
        same(call(Sign, NAN), Ok(0.0));
        same(call(Sign, -0.0), Ok(0.0));
        same(call(Sign, -3.0), Ok(-1.0));
        same(call(Sign, 0.1), Ok(1.0));
        same(call(Abs, -0.0), Ok(0.0));
        same(call(Abs, NAN), Ok(NAN));
    }

    #[test]
    fn power_checks_like_dpow() {
        same(power(0.0, -1.0), Err(ZeroToNegativePower));
        same(power(-8.0, 1.0 / 3.0), Err(ComplexPower));
        same(power(-1.0, 0.5), Err(ComplexPower));
        same(power(-8.0, 3.0), Ok(-512.0));
        same(power(-2.0, -1.0), Ok(-0.5));
        same(power(NAN, 0.0), Ok(1.0));
        same(power(1.0, NAN), Ok(1.0));
        same(power(NAN, NAN), Ok(NAN));
        same(power(0.0, NAN), Ok(NAN));
        same(power(-2.0, INF), Ok(INF));
        same(power(-1.0, INF), Ok(1.0));
        same(power(0.5, -INF), Ok(INF));
        same(power(2.0, -INF), Ok(0.0));
        same(power(-INF, 3.0), Ok(-INF));
        same(power(-INF, -3.0), Ok(-0.0));
        same(power(-INF, 2.0), Ok(INF));
        same(power(INF, -1.0), Ok(0.0));
        same(power(INF, 0.0), Ok(1.0));
        same(power(0.0, 0.0), Ok(1.0));
        same(power(2.0, 1e10), Err(FloatOverflow));
        same(power(0.5, 1e10), Err(FloatUnderflow));
        same(power(2.0, -1050.0), Ok(2f64.powf(-1050.0)));
        same(power(1e-160, 2.0), Ok(1e-160f64.powf(2.0)));
    }
}
