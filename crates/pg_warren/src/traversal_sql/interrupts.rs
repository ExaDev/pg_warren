//! The `Interrupts` poll on the Postgres main thread (DESIGN.md §6.9).

use warren_core::traversal::{Cancelled, Interrupts};

/// Polls Postgres for a pending cancel or termination. A pending interrupt
/// raises its `ERROR` (for example `query_canceled` on `statement_timeout`)
/// right here, which unwinds through the traversal: every buffer it holds
/// is owned, so nothing leaks. Main thread only.
pub(crate) struct PostgresInterrupts;

impl Interrupts for PostgresInterrupts {
    fn check(&mut self) -> Result<(), Cancelled> {
        pgrx::check_for_interrupts!();
        Ok(())
    }
}
