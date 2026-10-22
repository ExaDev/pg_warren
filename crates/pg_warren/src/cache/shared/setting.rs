//! `warren.shared_cache_memory` (DESIGN.md §5.6, §14 items 31 and 32): the
//! bytes of shared versions the cluster holds, loading, ready and draining
//! together; `0`, the default until M9's step 8, turns the shared cache
//! off, and a reload that sets it to `0` evicts what is shared
//! (`read::retire`).

use pgrx::{GucContext, GucFlags, GucRegistry, GucSetting};

/// `warren.shared_cache_memory`, in kB.
static SHARED_CACHE_MEMORY: GucSetting<i32> = GucSetting::<i32>::new(0);

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

/// `warren.shared_cache_memory`, in bytes: 0 when the shared cache is off.
pub(super) fn cap() -> u64 {
    u64::try_from(SHARED_CACHE_MEMORY.get()).unwrap_or(0) * 1024
}
