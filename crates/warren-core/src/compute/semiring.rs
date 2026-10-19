//! Semirings `(⊕, ⊗)` over vector elements, named as GraphBLAS names them
//! (DESIGN.md §6.3): `⊕` is a monoid whose identity is the "no entry" value
//! of a dense vector, and `⊗(a_ij, x_j)` takes the edge weight first and the
//! vector value second.

use std::cmp::Ordering;
use std::fmt::{self, Display};
use std::str::FromStr;

use super::cast::SqlCast;
use super::{ArithmeticError, ComputeError, Element};

/// A binary operator on elements of one type, as GraphBLAS names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    /// `first(a, b) = a`.
    First,
    /// `second(a, b) = b`.
    Second,
    /// `pair(a, b) = 1`.
    Pair,
    /// `a + b`; `lor` for `bool`.
    Plus,
    /// `a × b`; `land` for `bool`.
    Times,
    /// The smaller of `a` and `b` in SQL order (`NaN` above every number);
    /// `land` for `bool`.
    Min,
    /// The larger of `a` and `b` in SQL order; `lor` for `bool`.
    Max,
    /// `1` if either is non-zero, else `0`.
    Lor,
    /// `1` if both are non-zero, else `0`.
    Land,
    /// Either operand; pg_warren takes the first, so results are
    /// deterministic.
    Any,
}

impl BinaryOp {
    /// Every operator, in declaration order: `ALL[op as usize] == op`.
    pub(crate) const ALL: [Self; 10] = [
        Self::First,
        Self::Second,
        Self::Pair,
        Self::Plus,
        Self::Times,
        Self::Min,
        Self::Max,
        Self::Lor,
        Self::Land,
        Self::Any,
    ];

    /// The GraphBLAS name.
    pub fn name(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::Second => "second",
            Self::Pair => "pair",
            Self::Plus => "plus",
            Self::Times => "times",
            Self::Min => "min",
            Self::Max => "max",
            Self::Lor => "lor",
            Self::Land => "land",
            Self::Any => "any",
        }
    }
}

impl Display for BinaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for BinaryOp {
    type Err = ComputeError;

    fn from_str(name: &str) -> Result<Self, ComputeError> {
        Self::ALL
            .into_iter()
            .find(|op| op.name() == name)
            .ok_or_else(|| ComputeError::UnknownBinaryOp(name.to_owned()))
    }
}

/// The `⊕` of a semiring: a binary operator with an identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Monoid {
    /// `plus`, identity 0 (`false` for `bool`).
    Plus,
    /// `min`, identity +∞ (`i64::MAX`, `true` for `bool`).
    Min,
    /// `max`, identity −∞ (`i64::MIN`, `false` for `bool`).
    Max,
    /// `lor`, identity 0 / `false`.
    Lor,
    /// `any`, identity 0 / `false`.
    Any,
}

impl Monoid {
    /// The monoid's operator.
    pub fn op(self) -> BinaryOp {
        match self {
            Self::Plus => BinaryOp::Plus,
            Self::Min => BinaryOp::Min,
            Self::Max => BinaryOp::Max,
            Self::Lor => BinaryOp::Lor,
            Self::Any => BinaryOp::Any,
        }
    }
}

/// A v1 semiring `⊕.⊗` (DESIGN.md §6.3), named `<add>_<mult>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Semiring {
    /// `plus_times`, the default: `Σ a_ij × x_j`.
    PlusTimes,
    /// `min_plus`: shortest paths.
    MinPlus,
    /// `max_plus`: longest paths.
    MaxPlus,
    /// `max_times`.
    MaxTimes,
    /// `min_times`.
    MinTimes,
    /// `lor_land` (alias `or_and`): reachability.
    LorLand,
    /// `any_pair`: whether a row has any entry.
    AnyPair,
    /// `plus_pair`: the number of entries in a row.
    PlusPair,
    /// `plus_first`: the sum of a row's edge weights.
    PlusFirst,
    /// `plus_second`: the sum of the neighbours' values.
    PlusSecond,
    /// `min_first`: the smallest edge weight.
    MinFirst,
    /// `min_second`: the smallest neighbour value (min-label propagation).
    MinSecond,
    /// `max_second`: the largest neighbour value.
    MaxSecond,
}

impl Semiring {
    const ALL: [Self; 13] = [
        Self::PlusTimes,
        Self::MinPlus,
        Self::MaxPlus,
        Self::MaxTimes,
        Self::MinTimes,
        Self::LorLand,
        Self::AnyPair,
        Self::PlusPair,
        Self::PlusFirst,
        Self::PlusSecond,
        Self::MinFirst,
        Self::MinSecond,
        Self::MaxSecond,
    ];

    /// Every v1 semiring.
    pub fn all() -> impl Iterator<Item = Self> {
        Self::ALL.into_iter()
    }

    /// `⊕`.
    pub fn add(self) -> Monoid {
        match self {
            Self::PlusTimes | Self::PlusPair | Self::PlusFirst | Self::PlusSecond => Monoid::Plus,
            Self::MinPlus | Self::MinTimes | Self::MinFirst | Self::MinSecond => Monoid::Min,
            Self::MaxPlus | Self::MaxTimes | Self::MaxSecond => Monoid::Max,
            Self::LorLand => Monoid::Lor,
            Self::AnyPair => Monoid::Any,
        }
    }

    /// `⊗`, applied as `⊗(edge weight, vector value)`.
    pub fn multiply(self) -> BinaryOp {
        match self {
            Self::PlusTimes | Self::MaxTimes | Self::MinTimes => BinaryOp::Times,
            Self::MinPlus | Self::MaxPlus => BinaryOp::Plus,
            Self::LorLand => BinaryOp::Land,
            Self::AnyPair | Self::PlusPair => BinaryOp::Pair,
            Self::PlusFirst | Self::MinFirst => BinaryOp::First,
            Self::PlusSecond | Self::MinSecond | Self::MaxSecond => BinaryOp::Second,
        }
    }

    /// The canonical name.
    pub fn name(self) -> &'static str {
        match self {
            Self::PlusTimes => "plus_times",
            Self::MinPlus => "min_plus",
            Self::MaxPlus => "max_plus",
            Self::MaxTimes => "max_times",
            Self::MinTimes => "min_times",
            Self::LorLand => "lor_land",
            Self::AnyPair => "any_pair",
            Self::PlusPair => "plus_pair",
            Self::PlusFirst => "plus_first",
            Self::PlusSecond => "plus_second",
            Self::MinFirst => "min_first",
            Self::MinSecond => "min_second",
            Self::MaxSecond => "max_second",
        }
    }
}

impl Display for Semiring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Semiring {
    type Err = ComputeError;

    /// A semiring by canonical name, or `or_and` for `lor_land`.
    fn from_str(name: &str) -> Result<Self, ComputeError> {
        let canonical = match name {
            "or_and" => "lor_land",
            name => name,
        };
        Self::ALL
            .into_iter()
            .find(|semiring| semiring.name() == canonical)
            .ok_or_else(|| ComputeError::UnknownSemiring(name.to_owned()))
    }
}

/// An element type semirings compute over.
pub trait SemiringElement: Element {
    /// `op(a, b)`. Only `i64` can fail: an overflow is an error, as in SQL.
    fn apply(op: BinaryOp, a: Self, b: Self) -> Result<Self, ArithmeticError>;

    /// The identity of `monoid`: the value of "no entry".
    fn identity(monoid: Monoid) -> Self;

    /// An edge weight as this element type: `f32` and `i64` convert as
    /// SQL casts from `double precision` do (`i64` rounds half to even),
    /// failing out of range; `bool` is `weight ≠ 0`.
    fn from_weight(weight: f64) -> Result<Self, ArithmeticError>;

    /// `⊗(weight, value)` in a semiring whose `⊕` is `add`: `apply`, except
    /// that for `i64` the `min` identity `i64::MAX` and the `max` identity
    /// `i64::MIN` stand for ±∞ and absorb `plus`, and `times` by a weight
    /// above zero, so a node no edge has reached stays unreached across
    /// supersteps. `times` by a weight of zero or less treats the identity
    /// as an ordinary bigint. Other overflow is still an error.
    fn multiply(
        _add: BinaryOp,
        multiply: BinaryOp,
        weight: Self,
        value: Self,
    ) -> Result<Self, ArithmeticError> {
        Self::apply(multiply, weight, value)
    }
}

/// The smaller in SQL order; the first on a tie, as SQL `LEAST` keeps.
fn least<E: Element>(a: E, b: E) -> E {
    match b.sql_cmp(a) {
        Ordering::Less => b,
        _ => a,
    }
}

/// The larger in SQL order; the first on a tie, as SQL `GREATEST` keeps.
fn greatest<E: Element>(a: E, b: E) -> E {
    match b.sql_cmp(a) {
        Ordering::Greater => b,
        _ => a,
    }
}

macro_rules! float_semiring {
    ($type:ty) => {
        impl SemiringElement for $type {
            fn apply(op: BinaryOp, a: Self, b: Self) -> Result<Self, ArithmeticError> {
                let flag = |set: bool| if set { 1.0 } else { 0.0 };
                Ok(match op {
                    BinaryOp::First | BinaryOp::Any => a,
                    BinaryOp::Second => b,
                    BinaryOp::Pair => 1.0,
                    BinaryOp::Plus => a + b,
                    BinaryOp::Times => a * b,
                    BinaryOp::Min => least(a, b),
                    BinaryOp::Max => greatest(a, b),
                    BinaryOp::Lor => flag(a.is_nonzero() || b.is_nonzero()),
                    BinaryOp::Land => flag(a.is_nonzero() && b.is_nonzero()),
                })
            }

            fn identity(monoid: Monoid) -> Self {
                match monoid {
                    Monoid::Plus | Monoid::Lor | Monoid::Any => 0.0,
                    Monoid::Min => <$type>::INFINITY,
                    Monoid::Max => <$type>::NEG_INFINITY,
                }
            }

            fn from_weight(weight: f64) -> Result<Self, ArithmeticError> {
                weight.sql_cast()
            }
        }
    };
}

float_semiring!(f64);
float_semiring!(f32);

impl SemiringElement for i64 {
    fn apply(op: BinaryOp, a: Self, b: Self) -> Result<Self, ArithmeticError> {
        let overflow = ArithmeticError::BigintOutOfRange;
        Ok(match op {
            BinaryOp::First | BinaryOp::Any => a,
            BinaryOp::Second => b,
            BinaryOp::Pair => 1,
            BinaryOp::Plus => a.checked_add(b).ok_or(overflow)?,
            BinaryOp::Times => a.checked_mul(b).ok_or(overflow)?,
            BinaryOp::Min => a.min(b),
            BinaryOp::Max => a.max(b),
            BinaryOp::Lor => i64::from(a != 0 || b != 0),
            BinaryOp::Land => i64::from(a != 0 && b != 0),
        })
    }

    fn identity(monoid: Monoid) -> Self {
        match monoid {
            Monoid::Plus | Monoid::Lor | Monoid::Any => 0,
            Monoid::Min => i64::MAX,
            Monoid::Max => i64::MIN,
        }
    }

    fn multiply(
        add: BinaryOp,
        multiply: BinaryOp,
        weight: Self,
        value: Self,
    ) -> Result<Self, ArithmeticError> {
        let infinite = match add {
            BinaryOp::Min => value == i64::MAX,
            BinaryOp::Max => value == i64::MIN,
            _ => false,
        };
        let absorbs = match multiply {
            BinaryOp::Plus => true,
            // ±∞ · w is ±∞ only for w > 0; otherwise the identity is an
            // ordinary bigint (MAX · 0 = 0, MAX · −1 = −MAX, MIN · −1 and
            // |w| ≥ 2 overflow).
            BinaryOp::Times => weight > 0,
            _ => false,
        };
        match infinite && absorbs {
            true => Ok(value),
            false => Self::apply(multiply, weight, value),
        }
    }

    fn from_weight(weight: f64) -> Result<Self, ArithmeticError> {
        weight.sql_cast()
    }
}

impl SemiringElement for bool {
    fn apply(op: BinaryOp, a: Self, b: Self) -> Result<Self, ArithmeticError> {
        Ok(match op {
            BinaryOp::First | BinaryOp::Any => a,
            BinaryOp::Second => b,
            BinaryOp::Pair => true,
            BinaryOp::Plus | BinaryOp::Max | BinaryOp::Lor => a || b,
            BinaryOp::Times | BinaryOp::Min | BinaryOp::Land => a && b,
        })
    }

    fn identity(monoid: Monoid) -> Self {
        matches!(monoid, Monoid::Min)
    }

    fn from_weight(weight: f64) -> Result<Self, ArithmeticError> {
        weight.sql_cast()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [&str; 13] = [
        "plus_times",
        "min_plus",
        "max_plus",
        "max_times",
        "min_times",
        "lor_land",
        "any_pair",
        "plus_pair",
        "plus_first",
        "plus_second",
        "min_first",
        "min_second",
        "max_second",
    ];

    #[test]
    fn the_v1_semirings_parse_by_name() {
        for name in NAMES {
            let semiring: Semiring = name.parse().unwrap();
            assert_eq!(semiring.to_string(), name);
            let (add, multiply) = name.split_once('_').unwrap();
            assert_eq!(semiring.add().op().name(), add);
            assert_eq!(semiring.multiply().name(), multiply);
        }
        assert_eq!(Semiring::all().count(), NAMES.len());
        assert_eq!("or_and".parse::<Semiring>(), "lor_land".parse());
        assert_eq!("plus_times".parse::<Semiring>(), Ok(Semiring::PlusTimes));
    }

    #[test]
    fn unknown_names_are_errors() {
        assert_eq!(
            "plus_min".parse::<Semiring>(),
            Err(ComputeError::UnknownSemiring("plus_min".into()))
        );
        assert_eq!("".parse::<Semiring>(), Err(ComputeError::UnknownSemiring(String::new())));
        assert_eq!("minus".parse::<BinaryOp>(), Err(ComputeError::UnknownBinaryOp("minus".into())));
        for op in BinaryOp::ALL {
            assert_eq!(op.name().parse(), Ok(op));
        }
    }

    #[test]
    fn identities_are_the_no_entry_values() {
        let monoids = [Monoid::Plus, Monoid::Min, Monoid::Max, Monoid::Lor, Monoid::Any];
        let f64s = monoids.map(f64::identity);
        assert_eq!(f64s, [0.0, f64::INFINITY, f64::NEG_INFINITY, 0.0, 0.0]);
        assert_eq!(monoids.map(f32::identity), [0.0, f32::INFINITY, f32::NEG_INFINITY, 0.0, 0.0]);
        assert_eq!(monoids.map(i64::identity), [0, i64::MAX, i64::MIN, 0, 0]);
        assert_eq!(monoids.map(bool::identity), [false, true, false, false, false]);
    }

    fn table<E: SemiringElement>(a: E, b: E) -> Vec<E> {
        BinaryOp::ALL.iter().map(|&op| E::apply(op, a, b).unwrap()).collect()
    }

    #[test]
    fn binary_ops_on_each_element_type() {
        // first second pair plus times min max lor land any
        assert_eq!(table(2.0f64, -3.0), [2.0, -3.0, 1.0, -1.0, -6.0, -3.0, 2.0, 1.0, 1.0, 2.0]);
        assert_eq!(table(0.0f32, 4.0), [0.0, 4.0, 1.0, 4.0, 0.0, 0.0, 4.0, 1.0, 0.0, 0.0]);
        assert_eq!(table(5i64, 0), [5, 0, 1, 5, 0, 0, 5, 1, 0, 5]);
        assert_eq!(
            table(true, false),
            [true, false, true, true, false, false, true, true, false, true]
        );
    }

    #[test]
    fn min_and_max_order_nan_above_every_number() {
        let nan = f64::NAN;
        assert_eq!(f64::apply(BinaryOp::Min, nan, 1.0), Ok(1.0));
        assert!(f64::apply(BinaryOp::Max, 1.0, nan).unwrap().is_nan());
        assert_eq!(f64::apply(BinaryOp::Min, f64::INFINITY, nan), Ok(f64::INFINITY));
        assert_eq!(f64::apply(BinaryOp::Lor, nan, 0.0), Ok(1.0));
        // Ties keep the first operand, as SQL LEAST does.
        assert!(f64::apply(BinaryOp::Min, -0.0, 0.0).unwrap().is_sign_negative());
    }

    #[test]
    fn the_i64_min_and_max_identities_absorb_plus() {
        let (min, max, plus) = (BinaryOp::Min, BinaryOp::Max, BinaryOp::Plus);
        assert_eq!(i64::multiply(min, plus, 3, i64::MAX), Ok(i64::MAX));
        assert_eq!(i64::multiply(min, plus, -3, i64::MAX), Ok(i64::MAX));
        assert_eq!(i64::multiply(max, plus, 3, i64::MIN), Ok(i64::MIN));
        assert_eq!(i64::multiply(min, plus, 3, 4), Ok(7));
        // Genuine overflow is still an error, as is the other infinity.
        assert_eq!(i64::multiply(min, plus, 1, i64::MAX - 1), Ok(i64::MAX));
        assert_eq!(
            i64::multiply(min, plus, 2, i64::MAX - 1),
            Err(ArithmeticError::BigintOutOfRange)
        );
        assert_eq!(i64::multiply(max, plus, 1, i64::MAX), Err(ArithmeticError::BigintOutOfRange));
        assert_eq!(f64::multiply(min, plus, 1.0, f64::INFINITY), Ok(f64::INFINITY));
    }

    #[test]
    fn the_i64_min_and_max_identities_absorb_times_by_a_positive_weight() {
        let (min, max, times) = (BinaryOp::Min, BinaryOp::Max, BinaryOp::Times);
        assert_eq!(i64::multiply(min, times, 3, i64::MAX), Ok(i64::MAX));
        assert_eq!(i64::multiply(max, times, 3, i64::MIN), Ok(i64::MIN));
        assert_eq!(i64::multiply(min, times, 1, i64::MAX), Ok(i64::MAX));
        // Weights of zero or less treat the identity as an ordinary bigint.
        assert_eq!(i64::multiply(min, times, 0, i64::MAX), Ok(0));
        assert_eq!(i64::multiply(min, times, -1, i64::MAX), Ok(-i64::MAX));
        assert_eq!(i64::multiply(min, times, -2, i64::MAX), Err(ArithmeticError::BigintOutOfRange));
        assert_eq!(i64::multiply(max, times, -1, i64::MIN), Err(ArithmeticError::BigintOutOfRange));
        // Genuine overflow, and the other infinity, still error.
        assert_eq!(
            i64::multiply(min, times, 2, i64::MAX / 2 + 1),
            Err(ArithmeticError::BigintOutOfRange)
        );
        assert_eq!(i64::multiply(max, times, 2, i64::MAX), Err(ArithmeticError::BigintOutOfRange));
    }

    #[test]
    fn i64_overflow_is_an_error() {
        assert_eq!(i64::apply(BinaryOp::Plus, i64::MAX, 1), Err(ArithmeticError::BigintOutOfRange));
        assert_eq!(
            i64::apply(BinaryOp::Times, i64::MIN, -1),
            Err(ArithmeticError::BigintOutOfRange)
        );
    }

    #[test]
    fn weights_convert_as_sql_casts() {
        assert_eq!(f64::from_weight(0.1), Ok(0.1));
        assert_eq!(f32::from_weight(0.1), Ok(0.1f32));
        assert_eq!(f32::from_weight(1e300), Err(ArithmeticError::FloatOverflow));
        assert_eq!(f32::from_weight(1e-300), Err(ArithmeticError::FloatUnderflow));
        assert_eq!(f32::from_weight(f64::INFINITY), Ok(f32::INFINITY));
        assert_eq!(i64::from_weight(2.5), Ok(2));
        assert_eq!(i64::from_weight(3.5), Ok(4));
        assert_eq!(i64::from_weight(-2.5), Ok(-2));
        assert_eq!(i64::from_weight(9.3e18), Err(ArithmeticError::BigintOutOfRange));
        assert_eq!(i64::from_weight(f64::NAN), Err(ArithmeticError::BigintOutOfRange));
        assert_eq!(bool::from_weight(0.0), Ok(false));
        assert_eq!(bool::from_weight(f64::NAN), Ok(true));
    }
}
