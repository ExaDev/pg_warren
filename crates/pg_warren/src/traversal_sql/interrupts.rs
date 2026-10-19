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

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;
    use warren_core::graph::{EdgeList, GraphCsr};
    use warren_core::traversal::{components, Interrupts};

    use super::PostgresInterrupts;

    /// Marks a cancel as pending, as `statement_timeout` or
    /// `pg_cancel_backend` would, then runs `warren-core`'s components over
    /// a graph with the main-thread poll: the poll must raise
    /// `query_canceled`. Returns whether the traversal finished anyway.
    #[pg_extern]
    fn components_with_a_pending_cancel() -> bool {
        let graph = GraphCsr::build(EdgeList::new(false), 1, 1000, false);
        let Ok(graph) = graph else {
            return false;
        };
        // SAFETY: sets the flags the SIGINT handler sets; the next
        // CHECK_FOR_INTERRUPTS processes and clears them.
        unsafe {
            pg_sys::QueryCancelPending = 1;
            pg_sys::InterruptPending = 1;
        }
        let mut interrupts = PostgresInterrupts;
        let polled: &mut dyn Interrupts = &mut interrupts;
        components(&graph, polled).is_ok()
    }

    #[pg_test]
    fn a_pending_cancel_stops_a_traversal_with_query_canceled() -> Result<(), Error> {
        let cancelled = Spi::get_one::<bool>(
            "DO $$ BEGIN
                 PERFORM tests.components_with_a_pending_cancel();
                 RAISE EXCEPTION 'not cancelled';
             EXCEPTION WHEN query_canceled THEN NULL;
             END $$;
             SELECT true",
        )?;
        assert_eq!(cancelled, Some(true));
        Ok(())
    }
}
