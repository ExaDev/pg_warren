//! The decision on a write or a move of persisted vectors (DESIGN.md §14
//! item 47): admitted unless it takes a role's usage past its quota and
//! above what it was.

/// What a role stores on a graph before and after a write, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Usage {
    /// Before the write.
    pub(crate) before: u64,
    /// After it.
    pub(crate) after: u64,
}

impl Usage {
    /// Usage after writing `written` bytes in place of `replaced` bytes of
    /// the role's own (0 if the write replaces none), from `stored` bytes.
    pub(crate) fn of(stored: u64, replaced: u64, written: u64) -> Self {
        Self { before: stored, after: stored.saturating_sub(replaced).saturating_add(written) }
    }
}

/// Whether a write of `usage` is admitted under `quota` (bytes; `None`
/// for none), the writer `exempt` if a member of the owner role: unless it
/// takes usage past the quota and above what it was before, so replacing a
/// vector with one no larger always is, and a quota lowered below what is
/// in use blocks only growth.
pub(crate) fn admits(exempt: bool, quota: Option<u64>, usage: Usage) -> bool {
    exempt || quota.is_none_or(|quota| usage.after <= quota || usage.after <= usage.before)
}

#[cfg(test)]
mod tests {
    use super::{admits, Usage};

    #[test]
    fn a_write_within_the_quota_is_admitted() {
        assert!(admits(false, Some(1000), Usage::of(200, 0, 800)));
        assert!(!admits(false, Some(1000), Usage::of(200, 0, 801)));
    }

    #[test]
    fn without_a_quota_or_when_exempt_every_write_is_admitted() {
        assert!(admits(false, None, Usage::of(u64::MAX, 0, u64::MAX)));
        assert!(admits(true, Some(0), Usage::of(0, 0, 1)));
    }

    #[test]
    fn replacing_a_vector_with_one_no_larger_is_admitted_past_the_quota() {
        assert!(admits(false, Some(100), Usage::of(800, 800, 800)));
        assert!(admits(false, Some(100), Usage::of(800, 800, 400)));
        assert!(!admits(false, Some(100), Usage::of(800, 400, 800)));
    }

    #[test]
    fn a_quota_lowered_below_use_blocks_only_growth() {
        assert!(!admits(false, Some(500), Usage::of(800, 0, 1)));
        assert!(admits(false, Some(500), Usage::of(800, 100, 100)));
        assert!(!admits(false, Some(0), Usage::of(0, 0, 1)));
        assert!(admits(false, Some(0), Usage::of(0, 0, 0)));
    }

    #[test]
    fn usage_after_a_write_replaces_only_what_it_replaces() {
        assert_eq!(Usage::of(800, 300, 100), Usage { before: 800, after: 600 });
        assert_eq!(Usage::of(10, 20, 5), Usage { before: 10, after: 5 });
        assert_eq!(Usage::of(u64::MAX, 0, 1), Usage { before: u64::MAX, after: u64::MAX });
    }
}
