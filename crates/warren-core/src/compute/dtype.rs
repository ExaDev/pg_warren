//! The element types of vectors: `f32`, `f64`, `i64` and `bool`
//! (DESIGN.md §6.2, `dtype`).

use std::cmp::Ordering;
use std::fmt::{self, Debug, Display};
use std::str::FromStr;

use super::{AnyVector, ComputeError, Vector};

/// A vector's element type, named as the SQL `dtype` argument names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dtype {
    /// `f32`: SQL `real`.
    F32,
    /// `f64`: SQL `double precision`.
    F64,
    /// `i64`: SQL `bigint`.
    I64,
    /// `bool`: SQL `boolean`.
    Bool,
}

impl Dtype {
    /// The name `dtype` arguments use.
    pub fn name(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::I64 => "i64",
            Self::Bool => "bool",
        }
    }
}

impl Display for Dtype {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Dtype {
    type Err = ComputeError;

    fn from_str(name: &str) -> Result<Self, ComputeError> {
        [Self::F32, Self::F64, Self::I64, Self::Bool]
            .into_iter()
            .find(|dtype| dtype.name() == name)
            .ok_or_else(|| ComputeError::UnknownDtype(name.to_owned()))
    }
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
    impl Sealed for i64 {}
    impl Sealed for bool {}
}

/// A vector element type.
pub trait Element:
    Copy + Default + PartialEq + Debug + Send + Sync + 'static + sealed::Sealed
{
    /// This element type's [`Dtype`].
    const DTYPE: Dtype;

    /// SQL's total order: for floats, `NaN` equals `NaN` and sorts above
    /// every number, and `-0` equals `0`; `false < true`.
    fn sql_cmp(self, other: Self) -> Ordering;

    /// Whether the element casts to boolean true: any value but zero
    /// (`NaN` is non-zero), as a GraphBLAS value mask reads it.
    fn is_nonzero(self) -> bool;

    /// The vector, if `vector` holds this element type.
    fn vector(vector: &AnyVector) -> Option<&Vector<Self>>;

    /// The vector, mutably, if `vector` holds this element type.
    fn vector_mut(vector: &mut AnyVector) -> Option<&mut Vector<Self>>;
}

macro_rules! element {
    ($type:ty, $dtype:ident, $zero:expr, $cmp:expr) => {
        impl Element for $type {
            const DTYPE: Dtype = Dtype::$dtype;

            fn sql_cmp(self, other: Self) -> Ordering {
                $cmp(self, other)
            }

            fn is_nonzero(self) -> bool {
                self != $zero
            }

            fn vector(vector: &AnyVector) -> Option<&Vector<Self>> {
                match vector {
                    AnyVector::$dtype(vector) => Some(vector),
                    _ => None,
                }
            }

            fn vector_mut(vector: &mut AnyVector) -> Option<&mut Vector<Self>> {
                match vector {
                    AnyVector::$dtype(vector) => Some(vector),
                    _ => None,
                }
            }
        }

        impl From<Vector<$type>> for AnyVector {
            fn from(vector: Vector<$type>) -> Self {
                Self::$dtype(vector)
            }
        }
    };
}

element!(f32, F32, 0.0, |a: f32, b: f32| float_cmp(a.is_nan(), b.is_nan(), a.partial_cmp(&b)));
element!(f64, F64, 0.0, |a: f64, b: f64| float_cmp(a.is_nan(), b.is_nan(), a.partial_cmp(&b)));
element!(i64, I64, 0, |a: i64, b: i64| a.cmp(&b));
element!(bool, Bool, false, |a: bool, b: bool| a.cmp(&b));

/// SQL float order from the operands' NaN-ness and IEEE comparison.
fn float_cmp(a_nan: bool, b_nan: bool, ieee: Option<Ordering>) -> Ordering {
    match (a_nan, b_nan) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => ieee.unwrap_or(Ordering::Equal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dtypes_parse_from_their_names() {
        for dtype in [Dtype::F32, Dtype::F64, Dtype::I64, Dtype::Bool] {
            assert_eq!(dtype.to_string().parse::<Dtype>(), Ok(dtype));
        }
        assert_eq!("float8".parse::<Dtype>(), Err(ComputeError::UnknownDtype("float8".into())));
    }

    #[test]
    fn floats_order_as_sql_orders_them() {
        let nan = f64::NAN;
        assert_eq!(nan.sql_cmp(nan), Ordering::Equal);
        assert_eq!(nan.sql_cmp(f64::INFINITY), Ordering::Greater);
        assert_eq!(1.0f64.sql_cmp(nan), Ordering::Less);
        assert_eq!((-0.0f64).sql_cmp(0.0), Ordering::Equal);
        assert_eq!(f32::NAN.sql_cmp(1.0), Ordering::Greater);
        assert_eq!(false.sql_cmp(true), Ordering::Less);
        assert_eq!((-1i64).sql_cmp(1), Ordering::Less);
    }

    #[test]
    fn non_zero_includes_nan() {
        assert!(f64::NAN.is_nonzero());
        assert!(!(-0.0f32).is_nonzero());
        assert!(!0i64.is_nonzero());
        assert!(true.is_nonzero());
    }
}
