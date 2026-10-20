//! The backend's memory bound (DESIGN.md §5.5, §6.2, §14 items 18 and 20):
//! `warren.max_cache_memory`, the most heap memory this backend's cache
//! holds between statements (cached versions, cached persisted vectors and
//! transient vectors), and `warren.max_compute_memory`, what one call may
//! hold. Within a statement the cache may hold more than its cap: what the
//! statement has read is pinned until it ends (`statement`). What the cache
//! cannot evict (transient vectors, pinned entries a call is using, and
//! evicted versions a call still holds) is charged past the cap to each
//! call's compute budget, and a load that would take it past
//! `warren.max_compute_memory` is refused; pinned entries no call is using
//! give way, least recently used first, to a load or a call's budget that
//! needs the room. So the cache and a call together stay within the two
//! limits, a traversal's visited set and calls running at once aside.
//!
//! Unpinned reloadable entries (versions and persisted vectors) are
//! evicted to fit the cap, least recently used first; transient vectors
//! cannot be reloaded, so they are never evicted, and keeping one that
//! would take transient vectors alone past the cap is refused.

use pgrx::{GucContext, GucFlags, GucRegistry, GucSetting};
use warren_core::compute::ComputeMemory;

use super::lru::{admits, after, evictions, Evictions, Held};
use super::open::prune_dropped;
use super::statement::{vector_pinned, version_pinned};
use super::versions::{VersionCache, CACHE};
use super::CacheError;
use crate::catalog::VersionId;
use crate::vector::{
    cached_vector_bytes, cached_vectors, forget, role_transient_bytes, transient_bytes,
    vector_in_use, VectorId,
};

/// The default of both limits: 4 GiB, in kB.
const DEFAULT_KB: i32 = 4 * 1024 * 1024;

/// `warren.max_cache_memory`, in kB.
static MAX_CACHE_MEMORY: GucSetting<i32> = GucSetting::<i32>::new(DEFAULT_KB);

/// `warren.max_compute_memory`, in kB.
static MAX_COMPUTE_MEMORY: GucSetting<i32> = GucSetting::<i32>::new(DEFAULT_KB);

/// Registers `warren.max_cache_memory` and `warren.max_compute_memory`.
/// Called once, from `_PG_init`.
pub(crate) fn define_memory_settings() {
    GucRegistry::define_int_guc(
        c"warren.max_cache_memory",
        c"Memory this backend's warren cache may hold between statements: loaded graph \
           versions, cached vectors and transient vectors.",
        c"Least recently used versions and cached vectors are evicted to fit, and reloaded \
           when next used; what a statement has read stays until it ends, the excess charged \
           to warren.max_compute_memory; a transient vector that would take transient vectors \
           alone past it is refused with configuration_limit_exceeded.",
        &MAX_CACHE_MEMORY,
        64,
        i32::MAX,
        GucContext::Userset,
        GucFlags::UNIT_KB,
    );
    GucRegistry::define_int_guc(
        c"warren.max_compute_memory",
        c"Compute memory one warren.* call may hold: the vectors it reads and writes and \
           its work buffers, and what its statement holds in the cache past \
           warren.max_cache_memory.",
        c"A call that would exceed it fails with configuration_limit_exceeded before \
           allocating.",
        &MAX_COMPUTE_MEMORY,
        64,
        i32::MAX,
        GucContext::Userset,
        GucFlags::UNIT_KB,
    );
}

/// A setting in kB, in bytes.
fn bytes(setting: &GucSetting<i32>) -> u64 {
    u64::try_from(setting.get()).unwrap_or(0) * 1024
}

/// `warren.max_cache_memory`, in bytes.
fn max_cache_memory() -> u64 {
    bytes(&MAX_CACHE_MEMORY)
}

/// `warren.max_compute_memory`, in bytes.
fn max_compute_memory() -> u64 {
    bytes(&MAX_COMPUTE_MEMORY)
}

/// A fresh call budget of `warren.max_compute_memory`, less what the cache
/// holds past `warren.max_cache_memory` that it cannot evict. Made after
/// the call has opened its versions, so that their loads are counted and
/// they are in use. Pinned entries that no call uses first give way, least
/// recently used first, until the cache fits the cap beside what it cannot
/// evict: so the cache and the call together stay within the two limits.
pub(crate) fn compute_memory() -> ComputeMemory {
    let (held, cap) = (unevictable(), max_cache_memory());
    evict_idle_within(cap.max(held));
    ComputeMemory::with_held(max_compute_memory(), held.saturating_sub(cap))
}

/// The bytes the cache holds that cannot be evicted now: transient
/// vectors, versions evicted that calls still hold, and the versions and
/// persisted vectors the statement has pinned that a call is using.
fn unevictable() -> u64 {
    let held_by_calls = CACHE.with_borrow(VersionCache::held_by_calls_bytes);
    entries()
        .into_iter()
        .filter(|held| held.key.pinned() && held.key.in_use())
        .map(|held| held.bytes)
        .fold(transient_bytes().saturating_add(held_by_calls), u64::saturating_add)
}

/// Every reloadable entry the cache holds.
fn entries() -> Vec<Held<Reloadable>> {
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
    versions.chain(vectors).collect()
}

/// The bytes the cache holds in all.
fn cached_bytes() -> u64 {
    let versions = CACHE.with_borrow(|cache| cache.bytes() + cache.held_by_calls_bytes());
    versions.saturating_add(cached_vector_bytes()).saturating_add(transient_bytes())
}

/// Evicts the pinned entries no call is using, least recently used first,
/// until the cache fits in `within` bytes (or holds none of them).
fn evict_idle_within(within: u64) {
    let total = cached_bytes();
    if total <= within {
        return;
    }
    let idle: Vec<_> =
        entries().into_iter().filter(|held| held.key.pinned() && !held.key.in_use()).collect();
    let others = total.saturating_sub(idle.iter().map(|held| held.bytes).sum());
    evict(evictions(idle, others, within));
}

/// Refuses to load `versions`, about `expected` bytes in all, if what the
/// cache cannot evict would then exceed `warren.max_cache_memory` and
/// `warren.max_compute_memory` together: a call would have no compute
/// memory left. Otherwise makes room for them within the two: pinned
/// entries no call is using give way, least recently used first.
pub(super) fn admit_load(versions: &[VersionId], expected: u64) -> Result<(), CacheError> {
    let held = unevictable();
    let (cache_limit, compute_limit) = (max_cache_memory(), max_compute_memory());
    let both = cache_limit.saturating_add(compute_limit);
    if held.saturating_add(expected) > both {
        return Err(CacheError::StatementOverLimit {
            versions: versions.to_vec(),
            expected,
            held,
            cache_limit,
            compute_limit,
        });
    }
    evict_idle_within(both - expected);
    Ok(())
}

/// Whether a version of `expected` bytes could never be kept within
/// `warren.max_cache_memory` beside the transient vectors: the bytes held
/// by transient vectors and the cap, if so.
pub(super) fn unfit(expected: u64) -> Option<(u64, u64)> {
    let (transient, limit) = (transient_bytes(), max_cache_memory());
    (expected > limit.saturating_sub(transient)).then_some((transient, limit))
}

/// Evicts cached versions and persisted vectors that the statement has
/// not pinned until the cache fits `warren.max_cache_memory`: first any
/// that could never fit beside what cannot be evicted, then the least
/// recently used. Called after each load into the cache, each lookup and
/// at the end of each statement; callers holding an evicted entry keep it.
/// Within the cap it returns after summing the bytes held, without
/// allocating.
pub(crate) fn trim() {
    trim_within(max_cache_memory());
}

/// Before loading a version of about `expected` bytes, evicts as [`trim`]
/// does until the cache fits the cap with room for it, so the load takes
/// the cache past the cap only by what the statement cannot evict.
pub(super) fn make_room(expected: u64) {
    trim_within(max_cache_memory().saturating_sub(expected));
}

/// Evicts as [`trim`] does until the cache fits in `within` bytes, or holds
/// only what the statement has pinned and what cannot be evicted.
fn trim_within(within: u64) {
    let total = cached_bytes();
    if total <= within {
        return;
    }
    let evictable: Vec<_> = entries().into_iter().filter(|held| !held.key.pinned()).collect();
    let others = total.saturating_sub(evictable.iter().map(|held| held.bytes).sum());
    evict(evictions(evictable, others, within));
}

/// Evicts `evicted`.
fn evict(evicted: Evictions<Reloadable>) {
    for entry in evicted.unfit.into_iter().map(|unfit| unfit.key).chain(evicted.least_recent) {
        match entry {
            Reloadable::Version(version) => CACHE.with_borrow_mut(|cache| cache.remove(version)),
            Reloadable::Vector(id) => forget(id),
        }
    }
}

/// A reloadable cache entry.
#[derive(Debug, Clone, Copy)]
enum Reloadable {
    /// A cached version.
    Version(VersionId),
    /// A cached persisted vector.
    Vector(VectorId),
}

impl Reloadable {
    /// Whether the statement has pinned it.
    fn pinned(self) -> bool {
        match self {
            Self::Version(version) => version_pinned(version),
            Self::Vector(id) => vector_pinned(id),
        }
    }

    /// Whether a call is using it.
    fn in_use(self) -> bool {
        match self {
            Self::Version(version) => CACHE.with_borrow(|cache| cache.in_use(version)),
            Self::Vector(id) => vector_in_use(id),
        }
    }
}

/// The notice for version `version`, of `bytes`, which cannot be kept
/// within `limit` beside `transient` bytes of transient vectors.
pub(super) fn unfit_notice(version: VersionId, bytes: u64, limit: u64, transient: u64) -> String {
    format!(
        "version {version} ({bytes} bytes) cannot be kept within warren.max_cache_memory \
         ({limit} bytes, {transient} of them held by transient vectors), so it is held only \
         until the statement ends, the bytes past the cap charged to \
         warren.max_compute_memory, and reloaded by each statement that reads it; raise \
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
            after: after(in_use, replaced, added),
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

    use crate::cache::end_statement;
    use crate::fixture::{failure, refresh, ring, segment_reads};

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

    /// Runs each of `statements` as a statement of its own: a `#[pg_test]`
    /// is one statement, which keeps what it reads cached until it ends.
    fn statements(statements: &[&str]) -> Result<(), Error> {
        for statement in statements {
            Spi::run(statement)?;
            end_statement();
        }
        Ok(())
    }

    #[pg_test]
    fn a_second_graphs_load_evicts_the_first_under_a_small_cap() -> Result<(), Error> {
        rings(&["a", "b"])?;
        Spi::run("SET LOCAL warren.max_cache_memory = '150kB'")?;
        statements(&["SELECT warren.prewarm('a')", "SELECT warren.prewarm('b')"])?;
        assert_eq!(evict("a")?, Some(0));
        assert_eq!(evict("b")?, Some(1));
        Ok(())
    }

    #[pg_test]
    fn the_least_recently_used_graph_is_evicted_first() -> Result<(), Error> {
        rings(&["a", "b", "c"])?;
        Spi::run("SET LOCAL warren.max_cache_memory = '250kB'")?;
        statements(&[
            "SELECT warren.prewarm('a')",
            "SELECT warren.prewarm('b')",
            "SELECT warren.prewarm('a')",
            "SELECT warren.prewarm('c')",
        ])?;
        assert_eq!(evict("a")?, Some(1), "a was used after b");
        assert_eq!(evict("b")?, Some(0));
        assert_eq!(evict("c")?, Some(1));
        Ok(())
    }

    #[pg_test]
    fn a_graph_larger_than_the_cap_is_held_for_the_statement_but_not_kept() -> Result<(), Error> {
        rings(&["a"])?;
        Spi::run("SET LOCAL warren.max_cache_memory = '64kB'")?;
        let reached = Spi::get_one::<i64>(
            "SELECT count(*) FROM warren.reachable_set('a', ARRAY[1::bigint])",
        )?;
        assert_eq!(reached, Some(2000));
        Spi::run("SELECT warren.prewarm('a')")?;
        assert_eq!(evict("a")?, Some(1), "held until the statement ends");
        Spi::run("SELECT warren.prewarm('a')")?;
        end_statement();
        assert_eq!(evict("a")?, Some(0));
        Ok(())
    }

    #[pg_test]
    fn a_scan_holding_a_version_survives_its_eviction() -> Result<(), Error> {
        rings(&["a"])?;
        // Evicting a on the scan's first row frees nothing the scan holds.
        let reached = Spi::get_one::<i64>(
            "SELECT count(n) FROM (SELECT warren.reachable_set('a', ARRAY[1::bigint]) n,
                                          warren.evict('a') e) s",
        )?;
        assert_eq!(reached, Some(2000));
        assert_eq!(evict("a")?, Some(0));
        Ok(())
    }

    #[pg_test]
    fn cached_vectors_are_evicted_to_fit_and_reloaded_when_read() -> Result<(), Error> {
        rings(&["a"])?;
        // Five vectors of 16,000 bytes; three fit beside the ~94 kB
        // version in 150kB.
        Spi::run("SET LOCAL warren.max_cache_memory = '150kB'")?;
        for name in ["p", "q", "r", "s", "t"] {
            statements(&[&format!("SELECT warren.vector_const('a', '{name}', 1)")])?;
        }
        let mut sums = Vec::new();
        for name in ["p", "q", "r", "s", "t"] {
            sums.push(Spi::get_one::<f64>(&format!(
                "SELECT sum(value) FROM warren.vector_values('a', '{name}', NULL::bigint)"
            ))?);
            end_statement();
        }
        assert_eq!(sums, [Some(2000.0); 5]);
        assert_eq!(evict("a")?, Some(4), "the version and the three most recent vectors");
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
                "53400: cache memory limit exceeded: a transient vector of 16000 bytes would \
                 bring transient vectors to 80000 bytes, over warren.max_cache_memory (65536 \
                 bytes); 64000 are in use, 64000 of them this role's; persist it, drop \
                 transient vectors or raise warren.max_cache_memory"
            )
        );
        // Replacing one is not counted twice, and a persisted vector fits
        // nowhere but is still written.
        Spi::run("SELECT warren.vector_const('a', 'x', 2, persist := false)")?;
        Spi::run("SELECT warren.vector_const('a', 'p', 3)")?;
        let sum = "SELECT sum(value) FROM warren.vector_values('a', 'x', NULL::bigint)";
        assert_eq!(Spi::get_one::<f64>(sum)?, Some(4000.0));
        end_statement();
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
                "53400: cache memory limit exceeded: a transient vector of 16000 bytes would \
                 bring transient vectors to 80000 bytes, over warren.max_cache_memory (65536 \
                 bytes); 64000 are in use, 0 of them this role's; persist it, drop transient \
                 vectors or raise warren.max_cache_memory"
            )
        );
        Ok(())
    }

    #[pg_test]
    fn a_drop_rolled_back_frees_no_transient_vectors() -> Result<(), Error> {
        // (A committed drop frees them: test/sql/cache_dropped_transients.sql.)
        rings(&["a", "b"])?;
        Spi::run(
            "SELECT warren.vector_const('a', 'keep', 7, persist := false);
             DO $$ BEGIN
                 PERFORM warren.drop_graph('a');
                 PERFORM warren.evict('b');
                 RAISE EXCEPTION 'roll back';
             EXCEPTION WHEN raise_exception THEN NULL;
             END $$",
        )?;
        let sum = "SELECT sum(value) FROM warren.vector_values('a', 'keep', NULL::bigint)";
        assert_eq!(Spi::get_one::<f64>(sum)?, Some(14000.0));
        Ok(())
    }

    #[pg_test]
    fn a_transient_vector_may_be_replaced_by_no_larger_one_over_a_lowered_cap() -> Result<(), Error>
    {
        rings(&["a"])?;
        for name in ["v", "w", "x", "y", "z"] {
            Spi::run(&format!("SELECT warren.vector_const('a', '{name}', 1, persist := false)"))?;
        }
        Spi::run("SET LOCAL warren.max_cache_memory = '64kB'")?;
        let same = failure("warren.vector_const('a', 'v', 2, persist := false)")?;
        let smaller = failure("warren.vector_const('a', 'w', 2, 'f32', persist := false)")?;
        let larger = failure("warren.vector_const('a', 'u', 2, persist := false)")?;
        assert_eq!((same, smaller), (None, None));
        assert!(larger.unwrap_or_default().starts_with("53400"));
        Ok(())
    }

    #[pg_test]
    fn room_is_made_for_a_version_before_it_is_loaded() -> Result<(), Error> {
        rings(&["a", "b", "c"])?;
        Spi::run("SET LOCAL warren.max_cache_memory = '250kB'")?;
        statements(&["SELECT warren.prewarm('a')", "SELECT warren.prewarm('b')"])?;
        let c = Spi::get_one::<i64>("SELECT current_version FROM warren.graphs WHERE name = 'c'")?
            .ok_or(Error::InvalidPosition)?;
        let expected =
            super::super::open::expected_bytes::<i64>(c).map_err(|_| Error::InvalidPosition)?;
        assert!(expected > 90_000, "{expected}");
        // a, the least recently used, makes room for c.
        super::make_room(expected);
        assert_eq!(evict("a")?, Some(0));
        // For a version past the cap, nothing the statement has not read
        // stays.
        super::make_room(10_000_000);
        assert_eq!(evict("b")?, Some(0));
        Ok(())
    }

    #[pg_test]
    fn lowering_the_cap_trims_the_cache_at_the_next_lookup() -> Result<(), Error> {
        rings(&["a", "b"])?;
        statements(&["SELECT warren.prewarm('a')", "SELECT warren.prewarm('b')"])?;
        Spi::run("SET LOCAL warren.max_cache_memory = '150kB'; SELECT warren.prewarm('a')")?;
        assert_eq!(evict("a")?, Some(1), "the lookup marked a used");
        assert_eq!(evict("b")?, Some(0));
        Ok(())
    }

    #[pg_test]
    fn the_unfit_notice_names_the_entry_and_the_cap() {
        let notice = super::unfit_notice(12, 94_000, 65_536, 16_000);
        assert_eq!(
            notice,
            "version 12 (94000 bytes) cannot be kept within warren.max_cache_memory (65536 \
             bytes, 16000 of them held by transient vectors), so it is held only until the \
             statement ends, the bytes past the cap charged to warren.max_compute_memory, and \
             reloaded by each statement that reads it; raise warren.max_cache_memory"
        );
    }

    /// The M7 soak's cliff: a version that fits the cap alone, but not
    /// beside a library run's transient scratch, is loaded once per
    /// statement, not once per call.
    #[pg_test]
    fn a_library_run_loads_a_version_that_fits_only_without_its_scratch_once() -> Result<(), Error>
    {
        rings(&["a"])?;
        let before = segment_reads()?;
        Spi::run("SELECT warren.prewarm('a')")?;
        let one_load = segment_reads()? - before;
        assert!(one_load > 0);
        assert_eq!(evict("a")?, Some(1));
        // 94 kB of version fits 200kB, and so do pagerank's 176 kB of
        // scratch, but not both.
        Spi::run("SET LOCAL warren.max_cache_memory = '200kB'")?;
        let before = segment_reads()?;
        let iterations = Spi::get_one::<i32>("SELECT warren.pagerank('a', 'pr')")?;
        assert!(iterations >= Some(1));
        assert_eq!(segment_reads()? - before, one_load, "loaded once, not on every call");
        Ok(())
    }

    #[pg_test]
    fn what_a_statement_holds_past_the_cap_is_charged_to_the_compute_budget() -> Result<(), Error> {
        rings(&["a"])?;
        // A width-3 f64 vector is 48,000 bytes: within 64kB of compute
        // memory, but not beside the ~30 kB by which the version is held
        // past a 64kB cap. Within a 4GB cap, nothing is held past it.
        let wide = "warren.vector_from_query('a', 'w', 'SELECT id, ARRAY[1, 2, 3] FROM a_nodes')";
        Spi::run(
            "SET LOCAL warren.max_compute_memory = '64kB';
             SET LOCAL warren.max_cache_memory = '64kB'",
        )?;
        let refused = failure(wide)?.unwrap_or_default();
        Spi::run("SET LOCAL warren.max_cache_memory = '4GB'")?;
        assert_eq!(failure(wide)?, None);
        assert!(refused.starts_with("53400: compute memory limit exceeded"), "{refused}");
        Ok(())
    }

    /// A statement that reads more graphs, one after another, than both
    /// limits hold together: the versions no call still uses give way.
    #[pg_test]
    fn a_statement_reading_more_than_both_limits_in_sequence_evicts_what_no_call_uses(
    ) -> Result<(), Error> {
        rings(&["a", "b", "c", "d"])?;
        Spi::run(
            "SET LOCAL warren.max_cache_memory = '150kB';
             SET LOCAL warren.max_compute_memory = '64kB'",
        )?;
        let read = Spi::get_one::<i64>(
            "SELECT count(warren.prewarm(g)) FROM unnest(ARRAY['a', 'b', 'c', 'd']) g",
        )?;
        assert_eq!(read, Some(4));
        Ok(())
    }

    /// A vector the cache already counts is not charged again when a call
    /// reads it: ~94 kB of version and 32 kB of transient vectors fit the
    /// 128 kB the two limits give.
    #[pg_test]
    fn a_read_of_a_vector_the_cache_counts_is_not_charged_again() -> Result<(), Error> {
        rings(&["a"])?;
        Spi::run(
            "SET LOCAL warren.max_cache_memory = '64kB';
             SET LOCAL warren.max_compute_memory = '64kB';
             SELECT warren.vector_const('a', 'x', 1, persist := false);
             SELECT warren.vector_const('a', 'y', 1, persist := false)",
        )?;
        assert_eq!(failure("warren.vector_reduce('a', 'x', 'sum')")?, None);
        Ok(())
    }

    /// A version a call still holds after its eviction is memory too: it
    /// counts against both limits until the call lets it go.
    #[pg_test]
    fn a_version_a_call_holds_after_eviction_counts_against_both_limits() -> Result<(), Error> {
        rings(&["a", "b"])?;
        let resolved =
            super::super::resolve("a", None, "read").map_err(|_| Error::InvalidPosition)?;
        let held = super::super::cached::<i64>(&resolved).map_err(|_| Error::InvalidPosition)?;
        assert_eq!(evict("a")?, Some(1));
        Spi::run(
            "SET LOCAL warren.max_cache_memory = '64kB';
             SET LOCAL warren.max_compute_memory = '64kB'",
        )?;
        let refused = failure("warren.prewarm('b')")?.unwrap_or_default();
        assert!(refused.starts_with("53400: memory limit exceeded"), "{refused}");
        drop(held);
        assert_eq!(failure("warren.prewarm('b')")?, None, "once let go, it is not counted");
        Ok(())
    }
}
