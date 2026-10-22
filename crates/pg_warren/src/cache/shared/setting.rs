//! `warren.shared_cache_memory` (DESIGN.md §5.6, §14 items 31 and 32): the
//! bytes of shared versions the cluster holds, loading, ready and draining
//! together; on by default at 4GB (§14 item 32); `0` turns the shared
//! cache off, and a reload that sets it to `0` evicts what is shared
//! (`read::retire`).

use pgrx::{GucContext, GucFlags, GucRegistry, GucSetting};

/// The default of `warren.shared_cache_memory`: 4GB, in kB, as the other
/// limits' defaults (DESIGN.md §14 item 32).
const DEFAULT_KB: i32 = 4 * 1024 * 1024;

/// `warren.shared_cache_memory`, in kB.
static SHARED_CACHE_MEMORY: GucSetting<i32> = GucSetting::<i32>::new(DEFAULT_KB);

/// Registers `warren.shared_cache_memory`. Called once, from `_PG_init`.
pub(super) fn define() {
    GucRegistry::define_int_guc(
        c"warren.shared_cache_memory",
        c"Memory the cluster's shared warren cache may hold: graph versions loaded once \
           and mapped by every backend that reads them. 0 turns it off.",
        c"A version that does not fit is loaded into the reading backend's own cache, \
           within warren.max_cache_memory; unpinned versions are evicted least recently \
           used first to make room. Set to 0, the shared versions are evicted as backends \
           next read a graph, and freed once none maps them.",
        &SHARED_CACHE_MEMORY,
        0,
        i32::MAX,
        GucContext::Sighup,
        GucFlags::UNIT_KB,
    );
}

/// Whether `warren.shared_cache_memory` has its default value (whether or
/// not a configuration file sets it to that).
pub(super) fn at_default() -> bool {
    SHARED_CACHE_MEMORY.get() == DEFAULT_KB
}

/// `warren.shared_cache_memory`, in bytes: 0 when the shared cache is off.
pub(super) fn cap() -> u64 {
    u64::try_from(SHARED_CACHE_MEMORY.get()).unwrap_or(0) * 1024
}
