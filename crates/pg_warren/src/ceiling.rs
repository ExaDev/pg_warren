//! The administrator's ceilings (DESIGN.md §14 item 52): settings that
//! only the server's configuration sets (`SIGHUP`: `postgresql.conf`, a
//! managed service's parameter group, or `ALTER SYSTEM`) and that bound
//! what a session may ask through its own settings. A call uses the lower
//! of its session's setting and the ceiling; `-1`, the default, sets none,
//! and so does `0`. Also the cap on the compute threads of every backend
//! together, `warren.cluster_threads_limit`, which `interrupts` enforces.
//! Depends on no other feature.

use std::ffi::CStr;

use pgrx::{GucContext, GucFlags, GucRegistry, GucSetting};

/// A ceiling's value that sets none.
const NONE: i32 = -1;

/// The most `warren.threads_limit` may be: `warren.threads`' own maximum.
const MAX_THREADS: i32 = 1024;

/// The most `warren.cluster_threads_limit` may be.
const MAX_CLUSTER_THREADS: i32 = 65_536;

static THREADS: GucSetting<i32> = GucSetting::<i32>::new(NONE);
static COMPUTE_MEMORY: GucSetting<i32> = GucSetting::<i32>::new(NONE);
static CACHE_MEMORY: GucSetting<i32> = GucSetting::<i32>::new(NONE);
static BUILD_MEMORY: GucSetting<i32> = GucSetting::<i32>::new(NONE);
static WALK_PATHS: GucSetting<i32> = GucSetting::<i32>::new(NONE);
static CLUSTER_THREADS: GucSetting<i32> = GucSetting::<i32>::new(NONE);

/// A session setting's ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ceiling {
    /// `warren.threads_limit`, on `warren.threads`.
    Threads,
    /// `warren.compute_memory_limit`, on `warren.max_compute_memory`.
    ComputeMemory,
    /// `warren.cache_memory_limit`, on `warren.max_cache_memory`.
    CacheMemory,
    /// `warren.build_memory_limit`, on `warren.max_build_memory`.
    BuildMemory,
    /// `warren.walk_paths_limit`, on `warren.max_walk_paths`.
    WalkPaths,
}

/// What a call may use, and the setting that bounds it: the session's
/// own, or its ceiling, for an error to name the one to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bound {
    pub(crate) value: u64,
    pub(crate) setting: &'static str,
}

impl Ceiling {
    const ALL: [Self; 5] =
        [Self::Threads, Self::ComputeMemory, Self::CacheMemory, Self::BuildMemory, Self::WalkPaths];

    /// The ceiling's name.
    pub(crate) fn name(self) -> &'static str {
        self.c_name().to_str().unwrap_or_default()
    }

    fn c_name(self) -> &'static CStr {
        match self {
            Self::Threads => c"warren.threads_limit",
            Self::ComputeMemory => c"warren.compute_memory_limit",
            Self::CacheMemory => c"warren.cache_memory_limit",
            Self::BuildMemory => c"warren.build_memory_limit",
            Self::WalkPaths => c"warren.walk_paths_limit",
        }
    }

    /// The session setting it bounds.
    fn bounds(self) -> &'static str {
        match self {
            Self::Threads => "warren.threads",
            Self::ComputeMemory => "warren.max_compute_memory",
            Self::CacheMemory => "warren.max_cache_memory",
            Self::BuildMemory => "warren.max_build_memory",
            Self::WalkPaths => "warren.max_walk_paths",
        }
    }

    fn setting(self) -> &'static GucSetting<i32> {
        match self {
            Self::Threads => &THREADS,
            Self::ComputeMemory => &COMPUTE_MEMORY,
            Self::CacheMemory => &CACHE_MEMORY,
            Self::BuildMemory => &BUILD_MEMORY,
            Self::WalkPaths => &WALK_PATHS,
        }
    }

    /// Whether it is set in kB (and bounds bytes).
    fn in_kb(self) -> bool {
        matches!(self, Self::ComputeMemory | Self::CacheMemory | Self::BuildMemory)
    }

    /// `asked`, the session setting's value (in bytes for memory), within
    /// the ceiling.
    pub(crate) fn bound(self, asked: u64) -> Bound {
        let scale = if self.in_kb() { 1024 } else { 1 };
        lower(asked, self.bounds(), limit(self.setting().get(), scale), self.name())
    }

    /// Registers it.
    fn define(self) {
        let (max, flags) = match self {
            Self::Threads => (MAX_THREADS, GucFlags::default()),
            Self::WalkPaths => (i32::MAX, GucFlags::default()),
            _ => (i32::MAX, GucFlags::UNIT_KB),
        };
        GucRegistry::define_int_guc(
            self.c_name(),
            self.short_description(),
            c"Set in the server's configuration, not per session: a call uses the lower of \
               the session's setting and this. -1 (the default) or 0 sets no limit.",
            self.setting(),
            NONE,
            max,
            GucContext::Sighup,
            flags,
        );
    }

    fn short_description(self) -> &'static CStr {
        match self {
            Self::Threads => c"The most compute threads one call may run, whatever warren.threads.",
            Self::ComputeMemory => {
                c"The most compute memory one call may hold, whatever warren.max_compute_memory."
            }
            Self::CacheMemory => {
                c"The most a backend's cache may hold, whatever warren.max_cache_memory."
            }
            Self::BuildMemory => {
                c"The most memory a graph build may hold, whatever warren.max_build_memory."
            }
            Self::WalkPaths => {
                c"The largest max_results of a walk, whatever warren.max_walk_paths."
            }
        }
    }
}

/// A ceiling's value as a bound in the session setting's unit: none for
/// `-1` or `0`.
fn limit(value: i32, scale: u64) -> Option<u64> {
    let value = u64::try_from(value).ok().filter(|&value| value > 0)?;
    Some(value.saturating_mul(scale))
}

/// The lower of `asked` (from setting `asked_setting`) and `limit` (from
/// `limit_setting`), named by the setting it came from; the session's
/// setting where they are equal.
fn lower(
    asked: u64,
    asked_setting: &'static str,
    limit: Option<u64>,
    limit_setting: &'static str,
) -> Bound {
    match limit {
        Some(limit) if limit < asked => Bound { value: limit, setting: limit_setting },
        _ => Bound { value: asked, setting: asked_setting },
    }
}

/// `warren.cluster_threads_limit`: the most compute threads every
/// backend's calls may run at once, if set.
pub(crate) fn cluster_threads() -> Option<u32> {
    limit(CLUSTER_THREADS.get(), 1).and_then(|threads| u32::try_from(threads).ok())
}

/// Registers the ceilings and the cluster's cap. Called once, from
/// `_PG_init`.
pub(crate) fn define_ceilings() {
    for ceiling in Ceiling::ALL {
        ceiling.define();
    }
    GucRegistry::define_int_guc(
        c"warren.cluster_threads_limit",
        c"The most compute threads the calls of every backend may run at once.",
        c"-1, the default, or 0 sets no limit. A call runs on the \
           threads free, up to what it asks, and waits while none is. Needs pg_warren's \
           shared memory: on PostgreSQL 15 and 16, shared_preload_libraries.",
        &CLUSTER_THREADS,
        NONE,
        MAX_CLUSTER_THREADS,
        GucContext::Sighup,
        GucFlags::default(),
    );
}

#[cfg(test)]
mod bounds {
    use super::*;

    #[test]
    fn minus_one_and_zero_set_no_limit() {
        assert_eq!(limit(-1, 1024), None);
        assert_eq!(limit(0, 1024), None);
        assert_eq!(limit(3, 1024), Some(3072));
        assert_eq!(limit(i32::MAX, 1024), Some(i32::MAX as u64 * 1024));
    }

    #[test]
    fn a_call_uses_the_lower_and_names_the_setting_it_came_from() {
        let session = "warren.threads";
        let ceiling = "warren.threads_limit";
        assert_eq!(lower(8, session, Some(2), ceiling), Bound { value: 2, setting: ceiling });
        assert_eq!(lower(2, session, Some(8), ceiling), Bound { value: 2, setting: session });
        assert_eq!(lower(4, session, Some(4), ceiling), Bound { value: 4, setting: session });
        assert_eq!(lower(4, session, None, ceiling), Bound { value: 4, setting: session });
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::{set_in_configuration, Ceiling};

    #[pg_test]
    fn the_limits_are_set_only_in_the_configuration_and_default_to_none() -> Result<(), Error> {
        for ceiling in Ceiling::ALL {
            let name = ceiling.name();
            let row = Spi::get_two::<String, String>(&format!(
                "SELECT setting, context FROM pg_catalog.pg_settings WHERE name = '{name}'"
            ))?;
            assert_eq!(row, (Some("-1".into()), Some("sighup".into())), "{name}");
        }
        let cap = Spi::get_two::<String, String>(
            "SELECT boot_val, context FROM pg_catalog.pg_settings \
             WHERE name = 'warren.cluster_threads_limit'",
        )?;
        assert_eq!(cap, (Some("-1".into()), Some("sighup".into())));
        Ok(())
    }

    #[pg_test]
    fn a_ceiling_bounds_the_sessions_setting() -> Result<(), Error> {
        assert_eq!(Ceiling::Threads.bound(8).value, 8);
        set_in_configuration(c"warren.threads_limit", c"2");
        set_in_configuration(c"warren.compute_memory_limit", c"1MB");
        let threads = Ceiling::Threads.bound(8);
        let memory = Ceiling::ComputeMemory.bound(4 << 30);
        set_in_configuration(c"warren.threads_limit", c"-1");
        set_in_configuration(c"warren.compute_memory_limit", c"-1");
        assert_eq!((threads.value, threads.setting), (2, "warren.threads_limit"));
        assert_eq!((memory.value, memory.setting), (1 << 20, "warren.compute_memory_limit"));
        Ok(())
    }
}

/// Sets ceiling `name` to `value` in this backend, as a reload of the
/// configuration file would (it cannot be `SET`), for tests.
#[cfg(any(test, feature = "pg_test"))]
pub(crate) fn set_in_configuration(name: &CStr, value: &CStr) {
    // SAFETY: sets a setting by name on the main thread, in the context and
    // from the source of the configuration file.
    unsafe {
        pgrx::pg_sys::SetConfigOption(
            name.as_ptr(),
            value.as_ptr(),
            pgrx::pg_sys::GucContext::PGC_SIGHUP,
            pgrx::pg_sys::GucSource::PGC_S_FILE,
        );
    }
}
