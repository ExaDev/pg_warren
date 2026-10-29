//! `warren.max_walk_paths` (DESIGN.md §14 item 15): the ceiling on the rows
//! a walk that reports one row per walk may return, since the number of
//! walks can grow exponentially with the hops; an administrator's
//! `warren.walk_paths_limit` bounds it (§14 item 52).

use pgrx::{GucContext, GucFlags, GucRegistry, GucSetting};

use crate::ceiling::{Bound, Ceiling};

/// The rows such a walk returns when the caller gives no `max_results`.
pub(crate) const DEFAULT_WALK_ROWS: i64 = 100_000;

/// The default ceiling.
const DEFAULT_CEILING: i32 = 10_000_000;

/// `warren.max_walk_paths`.
static MAX_WALK_PATHS: GucSetting<i32> = GucSetting::<i32>::new(DEFAULT_CEILING);

/// Registers `warren.max_walk_paths`. Called once, from `_PG_init`.
pub(crate) fn define_max_walk_paths() {
    GucRegistry::define_int_guc(
        c"warren.max_walk_paths",
        c"The most rows warren.walk may be asked for when it reports a row per walk.",
        c"Applies to mode 'paths' and to 'ends' without distinct_ends; a larger max_results is \
           refused. Without max_results such a walk returns at most 100000 rows (or this \
           ceiling, if lower).",
        &MAX_WALK_PATHS,
        1,
        i32::MAX,
        GucContext::Userset,
        GucFlags::default(),
    );
}

/// The current ceiling: `warren.max_walk_paths`, within
/// `warren.walk_paths_limit`.
pub(crate) fn max_walk_paths() -> Bound {
    Ceiling::WalkPaths.bound(u64::try_from(MAX_WALK_PATHS.get()).unwrap_or(1))
}
