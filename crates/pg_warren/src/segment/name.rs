//! Segment names in `warren.segments.segment` (DESIGN.md §5.1, §5.3).

use std::fmt;

/// The `id_map` segment.
pub(crate) const ID_MAP: &str = "id_map";
/// The `tombstones` segment.
pub(crate) const TOMBSTONES: &str = "tombstones";
/// The `rel_dict` segment.
pub(crate) const RELATION_DICTIONARY: &str = "rel_dict";

/// Which CSR of a version a segment belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CsrScope {
    /// The CSR of the relation at this `rel_dict` index.
    Relation(usize),
    /// The merged (all-relations) CSR.
    Merged,
}

/// Out-edges or in-edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    /// The forward CSR.
    Forward,
    /// The reverse CSR, built by transposing the forward one.
    Reverse,
}

/// One of a CSR's three segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CsrPart {
    /// `offsets`.
    Offsets,
    /// `targets`.
    Targets,
    /// `weights`.
    Weights,
}

/// The name of one segment of one CSR, e.g. `rev_targets:2` or
/// `offsets:merged`.
pub(crate) fn csr_segment(direction: Direction, part: CsrPart, scope: CsrScope) -> String {
    let prefix = match direction {
        Direction::Forward => "",
        Direction::Reverse => "rev_",
    };
    let part = match part {
        CsrPart::Offsets => "offsets",
        CsrPart::Targets => "targets",
        CsrPart::Weights => "weights",
    };
    format!("{prefix}{part}:{scope}")
}

impl fmt::Display for CsrScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Relation(index) => write!(f, "{index}"),
            Self::Merged => f.write_str("merged"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csr_segments_are_named_by_direction_part_and_scope() {
        use CsrPart::*;
        assert_eq!(csr_segment(Direction::Forward, Offsets, CsrScope::Relation(0)), "offsets:0");
        assert_eq!(
            csr_segment(Direction::Reverse, Targets, CsrScope::Relation(12)),
            "rev_targets:12"
        );
        assert_eq!(csr_segment(Direction::Forward, Weights, CsrScope::Merged), "weights:merged");
    }
}
