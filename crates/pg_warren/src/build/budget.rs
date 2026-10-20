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
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, refresh, ring, versions};

    #[pg_test]
    fn max_build_memory_defaults_to_4gb() -> Result<(), Error> {
        let setting = Spi::get_one::<String>("SELECT current_setting('warren.max_build_memory')")?;
        assert_eq!(setting.as_deref(), Some("4GB"));
        Ok(())
    }

    /// A build over the budget is refused before its edge query runs (here
    /// it would fail with division by zero).
    #[pg_test]
    fn a_refresh_over_the_build_memory_budget_is_refused_before_reading_edges() -> Result<(), Error>
    {
        ring("budget", 70_000, 2)?;
        Spi::run(
            "UPDATE warren.graphs SET edge_query = 'SELECT src, dst FROM budget_edges WHERE 1 / 0 = 0'
             WHERE name = 'budget';
             SET LOCAL warren.max_build_memory = '1MB'",
        )?;
        let refused = failure("warren.refresh('budget')")?;
        assert!(
            refused.as_deref().is_some_and(|f| f.starts_with("53400: build memory limit exceeded")),
            "{refused:?}"
        );
        Ok(())
    }

    /// 70 000 nodes' forward and reverse offsets take 1.1 MB, within 2 MB,
    /// but the id map, the edge list and the CSRs take the build past it.
    #[pg_test]
    fn a_refresh_whose_whole_build_exceeds_the_budget_is_refused() -> Result<(), Error> {
        ring("budget", 70_000, 2)?;
        Spi::run("SET LOCAL warren.max_build_memory = '2MB'")?;
        let refused = failure("warren.refresh('budget')")?;
        assert!(
            refused.as_deref().is_some_and(|f| f.starts_with("53400: build memory limit exceeded")),
            "{refused:?}"
        );
        assert_eq!(versions("budget")?.as_deref(), Some(""));
        Ok(())
    }

    #[pg_test]
    fn a_refresh_within_the_budget_succeeds() -> Result<(), Error> {
        ring("budget", 70_000, 2)?;
        Spi::run("SET LOCAL warren.max_build_memory = '8MB'")?;
        refresh("budget", true).map(|_| ())
    }
}
