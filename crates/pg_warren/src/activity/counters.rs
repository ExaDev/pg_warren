//! What the cache records in this backend's slot (DESIGN.md §5.7): its
//! counters, once per statement and version or vector, as the statement
//! pin is taken, so per-row lookups neither inflate them nor pay for them;
//! and what its private cache holds and the shared bytes it maps. Pure
//! Rust, so they may be recorded at a statement's end.

use std::time::Duration;

use super::backend::record;
use super::slot::{Counters, Memory, Refused};

/// Where a version or vector a statement reads was found or loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Read {
    /// In this backend's own cache.
    PrivateHit,
    /// In the shared cache: an attach, or this backend's own mapping.
    SharedHit,
    /// Loaded into this backend's own cache.
    PrivateLoad,
    /// Loaded into the shared cache.
    SharedLoad,
}

/// Counts a read of a version or vector, found or loaded as `read`; a load
/// of `bytes` took `took`.
pub(crate) fn count_read(read: Read, bytes: u64, took: Duration) {
    record(|values| tally(&mut values.counters, read, bytes, took));
}

/// Counts `read` in `counters`.
fn tally(counters: &mut Counters, read: Read, bytes: u64, took: Duration) {
    counters.lookups += 1;
    let loaded = match read {
        Read::PrivateHit => {
            counters.private_hits += 1;
            false
        }
        Read::SharedHit => {
            counters.shared_hits += 1;
            false
        }
        Read::PrivateLoad => {
            counters.private_loads += 1;
            true
        }
        Read::SharedLoad => {
            counters.shared_loads += 1;
            true
        }
    };
    if loaded {
        counters.loaded_bytes = counters.loaded_bytes.saturating_add(bytes);
        let microseconds = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        counters.load_microseconds = counters.load_microseconds.saturating_add(microseconds);
    }
}

/// Counts a lookup that found its version loading in another backend.
pub(crate) fn count_wait() {
    record(|values| values.counters.waits += 1);
}

/// Counts a version or vector the shared cache did not hold, for `reason`.
pub(crate) fn count_refusal(reason: Refused) {
    record(|values| values.counters.refuse(reason));
}

/// Counts `count` entries evicted from this backend's own cache (trim,
/// reclaim, promotion, prune and `evict`).
pub(crate) fn count_private_evictions(count: u64) {
    if count > 0 {
        record(|values| values.counters.private_evictions += count);
    }
}

/// Counts `count` shared versions or vectors this backend evicted
/// (admission, drain, promotion, prune and `evict`).
pub(crate) fn count_shared_evictions(count: u64) {
    if count > 0 {
        record(|values| values.counters.shared_evictions += count);
    }
}

/// Records what this backend's private cache holds: everything in `held`
/// but the shared bytes it maps, which [`record_mapped`] records.
pub(crate) fn record_private(held: Memory) {
    record(|values| values.memory = Memory { mapped: values.memory.mapped, ..held });
}

/// Records the shared bytes this backend maps.
pub(crate) fn record_mapped(bytes: u64) {
    record(|values| values.memory.mapped = bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_read_is_a_lookup_and_loads_count_their_bytes_and_time() {
        let mut counters = Counters::default();
        tally(&mut counters, Read::PrivateHit, 0, Duration::ZERO);
        tally(&mut counters, Read::SharedHit, 0, Duration::ZERO);
        tally(&mut counters, Read::SharedLoad, 100, Duration::from_millis(3));
        tally(&mut counters, Read::PrivateLoad, 50, Duration::from_micros(7));
        assert_eq!(counters.lookups, 4);
        assert_eq!((counters.private_hits, counters.shared_hits), (1, 1));
        assert_eq!((counters.private_loads, counters.shared_loads), (1, 1));
        assert_eq!((counters.loaded_bytes, counters.load_microseconds), (150, 3007));
    }

    #[test]
    fn a_hit_counts_no_bytes() {
        let mut counters = Counters::default();
        tally(&mut counters, Read::PrivateHit, 999, Duration::from_secs(1));
        assert_eq!((counters.loaded_bytes, counters.load_microseconds), (0, 0));
    }
}
