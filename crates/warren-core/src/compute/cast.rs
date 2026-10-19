//! SQL casts between element types, as Postgres performs them: narrowing
//! float casts and float-to-integer casts fail rather than saturate.
//!
//! Postgres has no casts between `boolean` and the float types; pg_warren
//! defines them as for `bigint` (`true` is 1, non-zero is `true`).

use super::ArithmeticError;

/// A cast of `Self` to `To`.
pub(crate) trait SqlCast<To> {
    /// `self::To`, or the error Postgres raises for it.
    fn sql_cast(self) -> Result<To, ArithmeticError>;
}

macro_rules! cast {
    ($from:ty => $to:ty, $convert:expr) => {
        impl SqlCast<$to> for $from {
            fn sql_cast(self) -> Result<$to, ArithmeticError> {
                $convert(self)
            }
        }
    };
}

cast!(f64 => f64, Ok);
cast!(f64 => f32, float4_from_float8);
cast!(f64 => i64, int8_from_float8);
cast!(f64 => bool, |value: f64| Ok(value != 0.0));
cast!(f32 => f64, |value: f32| Ok(f64::from(value)));
cast!(f32 => f32, Ok);
cast!(f32 => i64, |value: f32| int8_from_float8(f64::from(value)));
cast!(f32 => bool, |value: f32| Ok(value != 0.0));
cast!(i64 => f64, |value: i64| Ok(value as f64));
cast!(i64 => f32, |value: i64| Ok(value as f32));
cast!(i64 => i64, Ok);
cast!(i64 => bool, |value: i64| Ok(value != 0));
cast!(bool => f64, |value: bool| Ok(f64::from(u8::from(value))));
cast!(bool => f32, |value: bool| Ok(f32::from(u8::from(value))));
cast!(bool => i64, |value: bool| Ok(i64::from(value)));
cast!(bool => bool, Ok);

/// `float8 → float4`: an error rather than a silent infinity or zero.
fn float4_from_float8(value: f64) -> Result<f32, ArithmeticError> {
    let narrowed = value as f32;
    if narrowed.is_infinite() && !value.is_infinite() {
        return Err(ArithmeticError::FloatOverflow);
    }
    if narrowed == 0.0 && value != 0.0 {
        return Err(ArithmeticError::FloatUnderflow);
    }
    Ok(narrowed)
}

/// `float8 → int8`: round half to even, then range-check.
fn int8_from_float8(value: f64) -> Result<i64, ArithmeticError> {
    let rounded = value.round_ties_even();
    // -2⁶³ is exact as a float; 2⁶³ is the first value past the range.
    let fits = (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&rounded);
    match fits {
        true => Ok(rounded as i64),
        false => Err(ArithmeticError::BigintOutOfRange),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cast<From: SqlCast<To>, To>(value: From) -> Result<To, ArithmeticError> {
        value.sql_cast()
    }

    #[test]
    fn narrowing_float_casts_fail_out_of_range() {
        assert_eq!(cast::<f64, f32>(0.1), Ok(0.1f32));
        assert_eq!(cast::<f64, f32>(1e39), Err(ArithmeticError::FloatOverflow));
        assert_eq!(cast::<f64, f32>(1e-50), Err(ArithmeticError::FloatUnderflow));
        assert_eq!(cast::<f64, f32>(1.5e-45), Ok(1e-45f32));
        assert!(cast::<f64, f32>(f64::NAN).unwrap().is_nan());
        assert_eq!(cast::<f64, f32>(f64::INFINITY), Ok(f32::INFINITY));
        assert_eq!(cast::<f32, f64>(0.1), Ok(0.10000000149011612));
    }

    #[test]
    fn float_to_integer_casts_round_half_to_even_and_fail_out_of_range() {
        assert_eq!(cast::<f64, i64>(2.5), Ok(2));
        assert_eq!(cast::<f64, i64>(-2.5), Ok(-2));
        assert_eq!(cast::<f32, i64>(3.5), Ok(4));
        assert_eq!(cast::<f64, i64>(9.3e18), Err(ArithmeticError::BigintOutOfRange));
        assert_eq!(cast::<f64, i64>(f64::NAN), Err(ArithmeticError::BigintOutOfRange));
        assert_eq!(cast::<f64, i64>(i64::MAX as f64), Err(ArithmeticError::BigintOutOfRange));
        assert_eq!(cast::<f64, i64>(i64::MIN as f64), Ok(i64::MIN));
    }

    #[test]
    fn integer_and_boolean_casts() {
        assert_eq!(cast::<i64, f32>(i64::MAX), Ok(9.223372e18));
        assert_eq!(cast::<i64, f64>(-3), Ok(-3.0));
        assert_eq!(cast::<bool, i64>(true), Ok(1));
        assert_eq!(cast::<i64, bool>(5), Ok(true));
        assert_eq!(cast::<f64, bool>(f64::NAN), Ok(true));
        assert_eq!(cast::<bool, f64>(false), Ok(0.0));
        assert_eq!(cast::<f32, bool>(-0.0), Ok(false));
        assert_eq!(cast::<bool, f32>(true), Ok(1.0));
    }
}
