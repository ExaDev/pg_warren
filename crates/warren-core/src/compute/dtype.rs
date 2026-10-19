//! The element types of vectors: `f32`, `f64`, `i64` and `bool`
//! (DESIGN.md §6.2, `dtype`).

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

    /// The vector, if `vector` holds this element type.
    fn vector(vector: &AnyVector) -> Option<&Vector<Self>>;

    /// The vector, mutably, if `vector` holds this element type.
    fn vector_mut(vector: &mut AnyVector) -> Option<&mut Vector<Self>>;
}

macro_rules! element {
    ($type:ty, $dtype:ident) => {
        impl Element for $type {
            const DTYPE: Dtype = Dtype::$dtype;

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

element!(f32, F32);
element!(f64, F64);
element!(i64, I64);
element!(bool, Bool);

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
}
