//! Segment names in `warren.segments.segment` (DESIGN.md §5.1, §5.3).

pub(crate) use warren_core::graph::{CsrDirection as Direction, CsrPart, CsrScope};

/// The `id_map` segment.
pub(crate) const ID_MAP: &str = "id_map";
/// The `tombstones` segment.
pub(crate) const TOMBSTONES: &str = "tombstones";
/// The `rel_dict` segment.
pub(crate) const RELATION_DICTIONARY: &str = "rel_dict";

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
    match scope {
        CsrScope::Relation(index) => format!("{prefix}{part}:{index}"),
        CsrScope::Merged => format!("{prefix}{part}:merged"),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
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
