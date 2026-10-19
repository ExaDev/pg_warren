//! The cache's eviction policy (DESIGN.md §5.5): which reloadable entries
//! (cached versions and persisted vectors) to evict, least recently used
//! first, so that what this backend's cache holds fits
//! `warren.max_cache_memory`; and whether a transient vector, which cannot
//! be reloaded and so is never evicted, may be kept.

use std::time::Instant;

/// A reloadable entry: its key, the heap bytes it holds, and when a call
/// last used it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Held<K> {
    /// What it is.
    pub(crate) key: K,
    /// The heap bytes it holds.
    pub(crate) bytes: u64,
    /// When it was last loaded or looked up.
    pub(crate) last_used: Instant,
}

/// The keys of the entries of `held` to evict so that the rest and the
/// `pinned` bytes (which cannot be evicted) fit in `limit`: first each
/// entry that could never fit beside the pinned bytes, so that it does not
/// push out the others, then the least recently used first; every entry,
/// if the pinned bytes alone do not fit.
pub(crate) fn evictions<K>(held: Vec<Held<K>>, pinned: u64, limit: u64) -> Vec<K> {
    let room = limit.saturating_sub(pinned);
    let (mut fitting, too_large): (Vec<_>, Vec<_>) =
        held.into_iter().partition(|entry| entry.bytes <= room);
    fitting.sort_by_key(|entry| entry.last_used);
    let mut total = fitting.iter().fold(pinned, |total, entry| total.saturating_add(entry.bytes));
    let least_recent = fitting.into_iter().take_while(|entry| {
        let over = total > limit;
        total -= entry.bytes;
        over
    });
    too_large.into_iter().chain(least_recent).map(|entry| entry.key).collect()
}

/// Whether transient vectors holding `in_use` bytes may keep one of
/// `added` bytes in place of one of `replaced` bytes (0 if new) within
/// `limit`.
pub(crate) fn admits(in_use: u64, replaced: u64, added: u64, limit: u64) -> bool {
    in_use.saturating_sub(replaced).saturating_add(added) <= limit
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Entry `key` of `bytes`, last used `at` seconds after `start`.
    fn held(start: Instant, key: char, bytes: u64, at: u64) -> Held<char> {
        Held { key, bytes, last_used: start + Duration::from_secs(at) }
    }

    #[test]
    fn nothing_is_evicted_within_the_limit() {
        let start = Instant::now();
        let entries = vec![held(start, 'a', 40, 1), held(start, 'b', 50, 2)];
        assert!(evictions(entries, 10, 100).is_empty());
    }

    #[test]
    fn the_least_recently_used_are_evicted_until_the_rest_fit() {
        let start = Instant::now();
        let entries = vec![
            held(start, 'c', 30, 3),
            held(start, 'a', 30, 1),
            held(start, 'd', 30, 4),
            held(start, 'b', 30, 2),
        ];
        assert_eq!(evictions(entries.clone(), 0, 90), ['a']);
        assert_eq!(evictions(entries.clone(), 0, 89), ['a', 'b']);
        assert_eq!(evictions(entries, 25, 90), ['a', 'b'], "pinned bytes count");
    }

    #[test]
    fn an_entry_that_can_never_fit_is_evicted_without_evicting_the_others() {
        let start = Instant::now();
        let entries =
            vec![held(start, 'a', 30, 1), held(start, 'b', 30, 2), held(start, 'c', 80, 3)];
        assert_eq!(evictions(entries.clone(), 0, 70), ['c']);
        assert_eq!(evictions(entries, 50, 100), ['c', 'a'], "nor beside the pinned bytes");
    }

    #[test]
    fn everything_is_evicted_when_the_pinned_bytes_alone_do_not_fit() {
        let start = Instant::now();
        let entries = vec![held(start, 'a', 10, 1), held(start, 'b', 10, 2)];
        assert_eq!(evictions(entries, 101, 100), ['a', 'b']);
    }

    #[test]
    fn a_transient_vector_is_admitted_only_within_the_limit() {
        assert!(admits(60, 0, 40, 100));
        assert!(!admits(60, 0, 41, 100));
        assert!(admits(60, 30, 70, 100), "the vector it replaces is not counted");
        assert!(!admits(0, 0, 101, 100));
    }
}
