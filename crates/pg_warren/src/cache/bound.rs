//! `warren.max_cache_memory` (DESIGN.md §5.5, §14 item 18): the most heap
//! memory this backend's cache holds between calls, counting its cached
//! versions, cached persisted vectors and transient vectors. Reloadable
//! entries (versions and persisted vectors) are evicted least recently
//! used first after each load into the cache; transient vectors cannot be
//! reloaded, so they are never evicted, and keeping one that would take
//! transient vectors alone past the cap is refused.

use std::cell::Cell;

use pgrx::{notice, pg_sys, GucContext, GucFlags, GucRegistry, GucSetting};

use super::lru::{admits, evictions, Held};
use super::open::prune_dropped;
use super::versions::{VersionCache, CACHE};
use super::CacheError;
use crate::catalog::VersionId;
use crate::vector::{
    cached_vector_bytes, cached_vectors, forget, role_transient_bytes, transient_bytes, VectorId,
};

/// The default cap: 4 GiB, in kB.
const DEFAULT_KB: i32 = 4 * 1024 * 1024;

/// `warren.max_cache_memory`, in kB.
static MAX_CACHE_MEMORY: GucSetting<i32> = GucSetting::<i32>::new(DEFAULT_KB);

/// Registers `warren.max_cache_memory`. Called once, from `_PG_init`.
pub(crate) fn define_max_cache_memory() {
    GucRegistry::define_int_guc(
        c"warren.max_cache_memory",
        c"Memory this backend's warren cache may hold between calls: loaded graph versions, \
           cached vectors and transient vectors.",
        c"Least recently used versions and cached vectors are evicted to fit, and reloaded \
           when next used; a transient vector that would take transient vectors alone past \
           it is refused with configuration_limit_exceeded.",
        &MAX_CACHE_MEMORY,
        64,
        i32::MAX,
        GucContext::Userset,
        GucFlags::UNIT_KB,
    );
}

/// `warren.max_cache_memory`, in bytes.
fn max_cache_memory() -> u64 {
    u64::try_from(MAX_CACHE_MEMORY.get()).unwrap_or(0) * 1024
}

/// A reloadable cache entry.
#[derive(Debug, Clone, Copy)]
enum Reloadable {
    /// A cached version.
    Version(VersionId),
    /// A cached persisted vector.
    Vector(VectorId),
}

/// Evicts cached versions and persisted vectors until the cache fits
/// `warren.max_cache_memory`: first any that could never fit beside the
/// transient vectors (with a notice, once per statement), then the least
/// recently used. Called after each load into the cache and each lookup;
/// callers holding an evicted entry keep it. Within the cap it returns
/// after summing the bytes held, without allocating.
pub(crate) fn trim() {
    let (pinned, limit) = (transient_bytes(), max_cache_memory());
    let cached = CACHE.with_borrow(VersionCache::bytes).saturating_add(cached_vector_bytes());
    if pinned.saturating_add(cached) <= limit {
        return;
    }
    let versions = CACHE.with_borrow(VersionCache::held).into_iter().map(|held| Held {
        key: Reloadable::Version(held.key),
        bytes: held.bytes,
        last_used: held.last_used,
    });
    let vectors = cached_vectors().into_iter().map(|(id, bytes, last_used)| Held {
        key: Reloadable::Vector(id),
        bytes,
        last_used,
    });
    let held = versions.chain(vectors).collect();
    let evicted = evictions(held, pinned, limit);
    for unfit in &evicted.unfit {
        if notice_due() {
            notice!("{}", unfit_notice(unfit.key, unfit.bytes, limit, pinned));
        }
    }
    for entry in evicted.unfit.into_iter().map(|unfit| unfit.key).chain(evicted.least_recent) {
        match entry {
            Reloadable::Version(version) => CACHE.with_borrow_mut(|cache| cache.remove(version)),
            Reloadable::Vector(id) => forget(id),
        }
    }
}

thread_local! {
    /// The start of the statement that last raised the unfit notice.
    static NOTICED: Cell<Option<pg_sys::TimestampTz>> = const { Cell::new(None) };
}

/// Whether the unfit notice is due: once per top-level statement, so a
/// loop that reloads an oversize graph on every call says so once.
fn notice_due() -> bool {
    // SAFETY: reads the backend's current statement start time.
    let statement = unsafe { pg_sys::GetCurrentStatementStartTimestamp() };
    NOTICED.replace(Some(statement)) != Some(statement)
}

/// The notice for `entry`, of `bytes`, evicted as soon as it was loaded
/// because it cannot fit within `limit` beside `pinned` bytes of transient
/// vectors.
fn unfit_notice(entry: Reloadable, bytes: u64, limit: u64, pinned: u64) -> String {
    let what = match entry {
        Reloadable::Version(version) => format!("version {version}"),
        Reloadable::Vector(id) => format!("vector {id}"),
    };
    format!(
        "{what} ({bytes} bytes) cannot be kept within warren.max_cache_memory ({limit} bytes, \
         {pinned} of them held by transient vectors), so it is reloaded on every use; raise \
         warren.max_cache_memory"
    )
}

/// Whether a transient vector of `added` bytes may be kept in place of one
/// of `replaced` bytes (0 if none): only if this backend's transient
/// vectors, of every role, stay within `warren.max_cache_memory`. Before
/// refusing, it drops the transient vectors of versions that no longer
/// exist, which may have been dropped without a load since to prune them.
pub(crate) fn admit_transient(replaced: u64, added: u64) -> Result<(), CacheError> {
    let limit = max_cache_memory();
    if admits(transient_bytes(), replaced, added, limit) {
        return Ok(());
    }
    prune_dropped()?;
    let in_use = transient_bytes();
    match admits(in_use, replaced, added, limit) {
        true => Ok(()),
        false => Err(CacheError::TransientOverLimit {
            requested: added,
            in_use,
            own: role_transient_bytes(),
            limit,
        }),
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, refresh, ring};

    /// Graphs `names`: rings of 2,000 nodes, each about 94 kB loaded.
    fn rings(names: &[&str]) -> Result<(), Error> {
        for name in names {
            ring(name, 2000, 1)?;
            refresh(name, true)?;
        }
        Ok(())
    }

    /// `warren.evict(name)`: how many entries of graph `name` the cache
    /// held.
    fn evict(name: &str) -> Result<Option<i64>, Error> {
        Spi::get_one::<i64>(&format!("SELECT warren.evict('{name}')"))
    }

    #[pg_test]
    fn the_cap_defaults_to_4gb_and_is_at_least_64kb() -> Result<(), Error> {
        let memory = Spi::get_one::<String>("SELECT current_setting('warren.max_cache_memory')")?;
        assert_eq!(memory.as_deref(), Some("4GB"));
        let refused = failure("set_config('warren.max_cache_memory', '63kB', true)")?;
        assert!(refused.unwrap_or_default().starts_with("22023"));
        assert_eq!(failure("set_config('warren.max_cache_memory', '64kB', true)")?, None);
        Ok(())
    }

    #[pg_test]
    fn a_second_graphs_load_evicts_the_first_under_a_small_cap() -> Result<(), Error> {
        rings(&["a", "b"])?;
        Spi::run(
            "SET LOCAL warren.max_cache_memory = '150kB';
             SELECT warren.prewarm('a'); SELECT warren.prewarm('b')",
        )?;
        assert_eq!(evict("a")?, Some(0));
        assert_eq!(evict("b")?, Some(1));
        Ok(())
    }

    #[pg_test]
    fn the_least_recently_used_graph_is_evicted_first() -> Result<(), Error> {
        rings(&["a", "b", "c"])?;
        Spi::run(
            "SET LOCAL warren.max_cache_memory = '250kB';
             SELECT warren.prewarm('a'); SELECT warren.prewarm('b');
             SELECT warren.prewarm('a'); SELECT warren.prewarm('c')",
        )?;
        assert_eq!(evict("a")?, Some(1), "a was used after b");
        assert_eq!(evict("b")?, Some(0));
        assert_eq!(evict("c")?, Some(1));
        Ok(())
    }

    #[pg_test]
    fn a_graph_larger_than_the_cap_is_loaded_for_the_call_but_not_kept() -> Result<(), Error> {
        rings(&["a"])?;
        Spi::run("SET LOCAL warren.max_cache_memory = '64kB'")?;
        let reached = Spi::get_one::<i64>(
            "SELECT count(*) FROM warren.reachable_set('a', ARRAY[1::bigint])",
        )?;
        assert_eq!(reached, Some(2000));
        assert_eq!(evict("a")?, Some(0));
        Ok(())
    }

    #[pg_test]
    fn a_scan_holding_a_version_survives_its_eviction() -> Result<(), Error> {
        rings(&["a", "b"])?;
        Spi::run("SET LOCAL warren.max_cache_memory = '150kB'")?;
        // Loading b on the scan's first row evicts a, which the scan holds.
        let reached = Spi::get_one::<i64>(
            "SELECT count(n) FROM (SELECT warren.reachable_set('a', ARRAY[1::bigint]) n,
                                          warren.prewarm('b') w) s",
        )?;
        assert_eq!(reached, Some(2000));
        assert_eq!(evict("a")?, Some(0));
        assert_eq!(evict("b")?, Some(1));
        Ok(())
    }

    #[pg_test]
    fn cached_vectors_are_evicted_to_fit_and_reloaded_when_read() -> Result<(), Error> {
        rings(&["a"])?;
        // Five vectors of 16,000 bytes; four fit. The version never does.
        Spi::run("SET LOCAL warren.max_cache_memory = '64kB'")?;
        for name in ["p", "q", "r", "s", "t"] {
            Spi::run(&format!("SELECT warren.vector_const('a', '{name}', 1)"))?;
        }
        let sums = Spi::get_one::<String>(
            "SELECT string_agg((SELECT sum(value) FROM warren.vector_values('a', n, NULL::bigint))::text, ' ')
             FROM unnest(ARRAY['p', 'q', 'r', 's', 't']) n",
        )?;
        assert_eq!(sums.as_deref(), Some("2000 2000 2000 2000 2000"));
        assert_eq!(evict("a")?, Some(4));
        Ok(())
    }

    #[pg_test]
    fn transient_vectors_are_never_evicted_and_refused_past_the_cap() -> Result<(), Error> {
        rings(&["a"])?;
        Spi::run("SET LOCAL warren.max_cache_memory = '64kB'")?;
        for name in ["w", "x", "y", "z"] {
            Spi::run(&format!("SELECT warren.vector_const('a', '{name}', 1, persist := false)"))?;
        }
        let refused = failure("warren.vector_const('a', 'v', 1, persist := false)")?;
        assert_eq!(
            refused.as_deref(),
            Some(
                "53400: cache memory limit exceeded: a transient vector of 16000 bytes \
                 requested, 64000 of 65536 in use by transient vectors (64000 of them this \
                 role's); persist it, drop transient vectors or raise warren.max_cache_memory"
            )
        );
        // Replacing one is not counted twice, and a persisted vector fits
        // nowhere but is still written.
        Spi::run("SELECT warren.vector_const('a', 'x', 2, persist := false)")?;
        Spi::run("SELECT warren.vector_const('a', 'p', 3)")?;
        let sum = "SELECT sum(value) FROM warren.vector_values('a', 'x', NULL::bigint)";
        assert_eq!(Spi::get_one::<f64>(sum)?, Some(4000.0));
        assert_eq!(evict("a")?, Some(4), "the four transient vectors, nothing else");
        Ok(())
    }

    #[pg_test]
    fn the_refusal_reports_the_callers_share_of_the_transient_vectors() -> Result<(), Error> {
        rings(&["a"])?;
        Spi::run("SET LOCAL warren.max_cache_memory = '64kB'")?;
        for name in ["w", "x", "y", "z"] {
            Spi::run(&format!("SELECT warren.vector_const('a', '{name}', 1, persist := false)"))?;
        }
        Spi::run(
            "CREATE ROLE other; SELECT warren.grant('a', 'other');
             CREATE TEMP TABLE failure (what text); GRANT ALL ON failure TO PUBLIC;
             SET ROLE other",
        )?;
        let refused = failure("warren.vector_const('a', 'v', 1, persist := false)")?;
        Spi::run("RESET ROLE")?;
        assert_eq!(
            refused.as_deref(),
            Some(
                "53400: cache memory limit exceeded: a transient vector of 16000 bytes \
                 requested, 64000 of 65536 in use by transient vectors (0 of them this \
                 role's); persist it, drop transient vectors or raise warren.max_cache_memory"
            )
        );
        Ok(())
    }

    #[pg_test]
    fn a_dropped_graphs_transient_vectors_do_not_block_others() -> Result<(), Error> {
        rings(&["a"])?;
        ring("b", 5, 1)?;
        refresh("b", true)?;
        Spi::run("SELECT warren.prewarm('b')")?;
        for name in ["v", "w", "x", "y", "z"] {
            Spi::run(&format!("SELECT warren.vector_const('a', '{name}', 1, persist := false)"))?;
        }
        Spi::run("SELECT warren.drop_graph('a'); SET LOCAL warren.max_cache_memory = '64kB'")?;
        // b's version is a cache hit, so no load has pruned a's vectors.
        assert_eq!(failure("warren.vector_const('b', 'q', 1, persist := false)")?, None);
        Ok(())
    }

    #[pg_test]
    fn lowering_the_cap_trims_the_cache_at_the_next_lookup() -> Result<(), Error> {
        rings(&["a", "b"])?;
        Spi::run(
            "SELECT warren.prewarm('a'); SELECT warren.prewarm('b');
             SET LOCAL warren.max_cache_memory = '150kB'; SELECT warren.prewarm('a')",
        )?;
        assert_eq!(evict("a")?, Some(1), "the lookup marked a used");
        assert_eq!(evict("b")?, Some(0));
        Ok(())
    }

    #[pg_test]
    fn the_unfit_notice_is_due_once_per_statement() {
        assert!(super::notice_due());
        assert!(!super::notice_due());
    }

    #[pg_test]
    fn the_unfit_notice_names_the_entry_and_the_cap() {
        let notice = super::unfit_notice(super::Reloadable::Version(12), 94_000, 65_536, 16_000);
        assert_eq!(
            notice,
            "version 12 (94000 bytes) cannot be kept within warren.max_cache_memory (65536 \
             bytes, 16000 of them held by transient vectors), so it is reloaded on every use; \
             raise warren.max_cache_memory"
        );
    }
}
