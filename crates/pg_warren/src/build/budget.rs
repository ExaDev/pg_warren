//! `warren.max_build_memory` (DESIGN.md §5.4, §14 item 17): the memory a
//! build's CSR offsets may take. Offsets grow as relations × id space, so a
//! build over the budget is refused before anything is allocated.

use pgrx::{GucContext, GucFlags, GucRegistry, GucSetting};

/// The default budget: 4 GiB, in kB (DESIGN.md §5.4: a 100M-edge graph
/// builds in under 4 GB).
const DEFAULT_KB: i32 = 4 * 1024 * 1024;

/// `warren.max_build_memory`, in kB.
static MAX_BUILD_MEMORY: GucSetting<i32> = GucSetting::<i32>::new(DEFAULT_KB);

/// Registers `warren.max_build_memory`. Called once, from `_PG_init`.
pub(crate) fn define_max_build_memory() {
    GucRegistry::define_int_guc(
        c"warren.max_build_memory",
        c"Memory a graph build's CSR offsets may take.",
        c"Offsets grow as relations times id space; a build whose offsets would \
           exceed this is refused before allocating.",
        &MAX_BUILD_MEMORY,
        1024,
        i32::MAX,
        GucContext::Userset,
        GucFlags::UNIT_KB,
    );
}

/// The current budget in bytes.
pub(crate) fn max_build_memory() -> u64 {
    u64::try_from(MAX_BUILD_MEMORY.get()).unwrap_or(0) * 1024
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{refresh, ring};

    #[pg_test]
    fn max_build_memory_defaults_to_4gb() -> Result<(), Error> {
        let setting = Spi::get_one::<String>("SELECT current_setting('warren.max_build_memory')")?;
        assert_eq!(setting.as_deref(), Some("4GB"));
        Ok(())
    }

    // 70 000 nodes: forward and reverse offsets take 2 × 70 001 × 8 bytes.
    #[pg_test(error = "CSR offsets need 1120016 bytes, over the budget of 1048576")]
    fn a_refresh_over_the_build_memory_budget_is_refused() -> Result<(), Error> {
        ring("budget", 70_000, 2)?;
        Spi::run("SET LOCAL warren.max_build_memory = '1MB'")?;
        refresh("budget", true).map(|_| ())
    }

    #[pg_test]
    fn a_refresh_within_the_budget_succeeds() -> Result<(), Error> {
        ring("budget", 70_000, 2)?;
        Spi::run("SET LOCAL warren.max_build_memory = '2MB'")?;
        refresh("budget", true).map(|_| ())
    }
}
