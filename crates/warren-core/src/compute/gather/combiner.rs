//! Combiners: how `gather` folds the messages a receiver gets into its
//! value (DESIGN.md §6.4, `combine`).

use std::fmt::{self, Display};
use std::str::FromStr;

use crate::compute::{ComputeError, Dtype, MessageKind};

/// How a receiver's messages are combined, named as `combine` names it.
/// Messages are combined in collect order: ascending sender dense id, and a
/// sender's own messages in the order they were pushed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Combiner {
    /// The list of messages received, in collect order. Any message kind.
    Collect,
    /// The number of messages received, as an `i64`. Any message kind.
    Count,
    /// The sum, folded left in collect order: `f32`, `f64` or `i64`
    /// messages (`i64` overflow is an error, `bigint out of range`).
    Sum,
    /// The smallest in SQL order (`NaN` above every number): `f32`, `f64`
    /// or `i64` messages.
    Min,
    /// The largest in SQL order: `f32`, `f64` or `i64` messages.
    Max,
    /// The first message in collect order. Any message kind.
    First,
    /// The bytes of every message, concatenated in collect order: bytes
    /// messages only.
    Concat,
    /// Whether any message is true: `bool` messages only.
    BoolOr,
    /// Whether every message is true: `bool` messages only.
    BoolAnd,
}

impl Combiner {
    const ALL: [Self; 9] = [
        Self::Collect,
        Self::Count,
        Self::Sum,
        Self::Min,
        Self::Max,
        Self::First,
        Self::Concat,
        Self::BoolOr,
        Self::BoolAnd,
    ];

    /// Every combiner.
    pub fn all() -> impl Iterator<Item = Self> {
        Self::ALL.into_iter()
    }

    /// The name `combine` uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::Collect => "collect",
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Min => "min",
            Self::Max => "max",
            Self::First => "first",
            Self::Concat => "concat",
            Self::BoolOr => "bool_or",
            Self::BoolAnd => "bool_and",
        }
    }

    /// Checks that the combiner accepts messages of `kind`.
    pub fn check(self, kind: MessageKind) -> Result<(), ComputeError> {
        let numeric = matches!(kind, MessageKind::Element(Dtype::F32 | Dtype::F64 | Dtype::I64));
        let accepted = match self {
            Self::Collect | Self::Count | Self::First => true,
            Self::Sum | Self::Min | Self::Max => numeric,
            Self::BoolOr | Self::BoolAnd => kind == MessageKind::Element(Dtype::Bool),
            Self::Concat => kind == MessageKind::Bytes,
        };
        match accepted {
            true => Ok(()),
            false => Err(ComputeError::CombinerType { combiner: self, message: kind }),
        }
    }
}

impl Display for Combiner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Combiner {
    type Err = ComputeError;

    fn from_str(name: &str) -> Result<Self, ComputeError> {
        Self::ALL
            .into_iter()
            .find(|combiner| combiner.name() == name)
            .ok_or_else(|| ComputeError::UnknownCombiner(name.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [&str; 9] =
        ["collect", "count", "sum", "min", "max", "first", "concat", "bool_or", "bool_and"];

    #[test]
    fn combiners_parse_from_their_names() {
        for name in NAMES {
            let combiner: Combiner = name.parse().unwrap();
            assert_eq!(combiner.to_string(), name);
        }
        assert_eq!(Combiner::all().count(), NAMES.len());
        assert_eq!(
            "array_agg".parse::<Combiner>(),
            Err(ComputeError::UnknownCombiner("array_agg".into()))
        );
    }

    #[test]
    fn each_combiner_accepts_the_message_kinds_sql_would() {
        let kinds = [
            MessageKind::Element(Dtype::F32),
            MessageKind::Element(Dtype::F64),
            MessageKind::Element(Dtype::I64),
            MessageKind::Element(Dtype::Bool),
            MessageKind::Bytes,
        ];
        let accepted = |combiner: Combiner| -> Vec<bool> {
            kinds.iter().map(|&kind| combiner.check(kind).is_ok()).collect()
        };
        for any in [Combiner::Collect, Combiner::Count, Combiner::First] {
            assert_eq!(accepted(any), [true; 5]);
        }
        for numeric in [Combiner::Sum, Combiner::Min, Combiner::Max] {
            assert_eq!(accepted(numeric), [true, true, true, false, false]);
        }
        for boolean in [Combiner::BoolOr, Combiner::BoolAnd] {
            assert_eq!(accepted(boolean), [false, false, false, true, false]);
        }
        assert_eq!(accepted(Combiner::Concat), [false, false, false, false, true]);
        assert_eq!(
            Combiner::Sum.check(MessageKind::Bytes),
            Err(ComputeError::CombinerType {
                combiner: Combiner::Sum,
                message: MessageKind::Bytes
            })
        );
        assert_eq!(
            Combiner::Sum.check(MessageKind::Bytes).unwrap_err().to_string(),
            "combine := sum does not accept bytes messages"
        );
    }
}
