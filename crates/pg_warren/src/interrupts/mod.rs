//! The Postgres adapter of `warren-core`'s `Interrupts` port: the poll
//! traversals and compute kernels make on the main thread (DESIGN.md §6.9,
//! §9.3), and the compute pool whose jobs it polls for (`pool`); and of
//! its `Progress` port, which writes what they report to the open
//! operation's frame (`activity`, §5.7). Builds, traversal and compute
//! bindings use it; it depends only on `activity`.

mod pool;

pub(crate) use pool::{define_compute_settings, workers};

use warren_core::interrupts::{Cancelled, Interrupts};
use warren_core::progress::Progress;

use crate::activity;

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

    fn progress(&mut self) -> Option<&mut dyn Progress> {
        Some(self)
    }
}

/// Reports to the open operation, if any: pure Rust over this backend's
/// own slot.
impl Progress for PostgresInterrupts {
    fn phase(&mut self, name: &'static str, total: Option<u64>) {
        activity::phase(name, total);
    }

    fn done(&mut self, done: u64) {
        activity::done(done);
    }

    fn detail(&mut self, name: &'static str, value: f64) {
        activity::detail(name, value);
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;
    use warren_core::graph::{EdgeList, GraphCsr};
    use warren_core::interrupts::Interrupts;
    use warren_core::traversal::components;

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
