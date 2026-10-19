//! Retention (DESIGN.md §5.4 step 6, §7): which promoted versions a
//! promotion deletes. The current version counts towards `retain_versions`
//! and is never deleted; staged versions are never deleted; a version with
//! bound vectors is kept, and reported.

use super::VersionId;

/// What retention needs to know about one version of a graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VersionSummary {
    /// The version.
    pub(crate) version: VersionId,
    /// Its sequence number in the graph.
    pub(crate) seq: i64,
    /// Whether it is staged (never promoted).
    pub(crate) staged: bool,
    /// The number of named vectors bound to it.
    pub(crate) bound_vectors: i64,
}

/// The outcome of retention.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Retention {
    /// Promoted versions beyond the limit, newest first.
    pub(crate) delete: Vec<VersionId>,
    /// Versions beyond the limit kept because vectors are bound to them,
    /// with the number of vectors.
    pub(crate) kept_for_vectors: Vec<(VersionId, i64)>,
}

/// Retention of `versions` once `current` is the current version: keep
/// `current` and the `retain_versions - 1` newest (by `seq`) other promoted
/// versions; delete the other promoted versions unless vectors are bound
/// to them.
pub(crate) fn retention(
    versions: &[VersionSummary],
    current: VersionId,
    retain_versions: i32,
) -> Retention {
    let mut others: Vec<_> =
        versions.iter().filter(|v| !v.staged && v.version != current).collect();
    others.sort_by_key(|v| std::cmp::Reverse(v.seq));
    let keep = usize::try_from(retain_versions.saturating_sub(1)).unwrap_or(0);
    let mut outcome = Retention::default();
    for version in others.into_iter().skip(keep) {
        match version.bound_vectors {
            0 => outcome.delete.push(version.version),
            bound => outcome.kept_for_vectors.push((version.version, bound)),
        }
    }
    outcome
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use super::*;

    fn promoted(version: VersionId) -> VersionSummary {
        VersionSummary { version, seq: version, staged: false, bound_vectors: 0 }
    }

    #[test]
    fn the_current_version_and_the_newest_others_are_kept() {
        let versions: Vec<_> = (1..=5).map(promoted).collect();
        assert_eq!(
            retention(&versions, 5, 3),
            Retention { delete: vec![2, 1], kept_for_vectors: vec![] }
        );
    }

    #[test]
    fn retaining_one_version_keeps_only_the_current() {
        let versions: Vec<_> = (1..=3).map(promoted).collect();
        assert_eq!(retention(&versions, 3, 1).delete, [2, 1]);
    }

    #[test]
    fn an_older_current_version_is_kept_and_counts_towards_the_limit() {
        let versions: Vec<_> = (1..=4).map(promoted).collect();
        assert_eq!(retention(&versions, 1, 2).delete, [3, 2]);
    }

    #[test]
    fn staged_versions_are_never_deleted_nor_counted() {
        let mut versions: Vec<_> = (1..=4).map(promoted).collect();
        versions[1].staged = true;
        versions[3].staged = true;
        assert_eq!(retention(&versions, 3, 1).delete, [1]);
    }

    #[test]
    fn a_version_with_bound_vectors_is_kept_and_reported() {
        let mut versions: Vec<_> = (1..=4).map(promoted).collect();
        versions[0].bound_vectors = 2;
        assert_eq!(
            retention(&versions, 4, 2),
            Retention { delete: vec![2], kept_for_vectors: vec![(1, 2)] }
        );
    }

    #[test]
    fn nothing_is_deleted_within_the_limit() {
        let versions: Vec<_> = (1..=2).map(promoted).collect();
        assert_eq!(retention(&versions, 2, 2), Retention::default());
    }
}
