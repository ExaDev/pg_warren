//! Tests of the compute limits (DESIGN.md §6.9, §12): the memory cap is an
//! error, never an out-of-memory abort, and a cancel stops a kernel that
//! runs on the compute threads.

#[pgrx::pg_schema]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::compute_sql::settings::workers;
    use crate::fixture::{failure, refresh, ring};
    use crate::interrupts::PostgresInterrupts;

    #[pg_test]
    fn the_compute_memory_cap_is_an_error_not_an_abort() -> Result<(), Error> {
        ring("big", 20_000, 2)?;
        refresh("big", true)?;
        Spi::run("SET LOCAL warren.max_compute_memory = '64kB'")?;
        let refused = failure("warren.vector_const('big', 'x', 1)")?;
        assert_eq!(
            refused.as_deref(),
            Some("53400: compute memory limit exceeded: 160000 bytes requested, 0 of 65536 in use")
        );
        Spi::run("SET LOCAL warren.max_compute_memory = '400kB'; SELECT warren.vector_const('big', 'x', 1)")?;
        // x (160 000 bytes) plus y: within 400 kB, but a third vector is not.
        Spi::run("SELECT warren.spmv('big', 'x', 'y')")?;
        let refused = failure("warren.vector_map('big', 'z', 'x + y')")?.unwrap_or_default();
        assert!(refused.starts_with("53400: compute memory limit exceeded"), "{refused}");
        Ok(())
    }

    #[pg_test]
    fn a_pending_cancel_stops_spmv_with_query_canceled() -> Result<(), Error> {
        ring("big", 1000, 2)?;
        refresh("big", true)?;
        Spi::run("SELECT warren.vector_const('big', 'x', 1)")?;
        Spi::run(
            "CREATE FUNCTION pg_temp.cancel_then_spmv() RETURNS void LANGUAGE plpgsql AS $$
             BEGIN
                 PERFORM tests.mark_cancel_pending();
                 PERFORM warren.spmv('big', 'x', 'y');
             END $$",
        )?;
        assert_eq!(canceled("pg_temp.cancel_then_spmv()")?, Some(true));
        Ok(())
    }

    /// Marks a cancel as pending, as `statement_timeout` or
    /// `pg_cancel_backend` would.
    /// Whether `PERFORM <expression>` raises `query_canceled` (which
    /// `WHEN OTHERS` does not catch).
    fn canceled(expression: &str) -> Result<Option<bool>, Error> {
        Spi::get_one::<bool>(&format!(
            "DO $do$ BEGIN
                 PERFORM {expression};
                 RAISE EXCEPTION 'not cancelled';
             EXCEPTION WHEN query_canceled THEN NULL;
             END $do$;
             SELECT true"
        ))
    }

    #[pg_extern]
    fn mark_cancel_pending() {
        // SAFETY: sets the flags the SIGINT handler sets; the next
        // CHECK_FOR_INTERRUPTS processes and clears them.
        unsafe {
            pg_sys::QueryCancelPending = 1;
            pg_sys::InterruptPending = 1;
        }
    }

    /// Whether the job below saw its cancellation flag raised.
    static SAW_CANCELLATION: AtomicBool = AtomicBool::new(false);

    /// Runs a job on the compute pool that, mid-run, marks a cancel as
    /// pending (as a signal handler would) and then waits for the job's
    /// cancellation flag: the main thread's poll must raise
    /// `query_canceled`, and stop the job.
    #[pg_extern]
    fn cancel_a_running_job() {
        SAW_CANCELLATION.store(false, Ordering::Relaxed);
        let Ok(pool) = workers() else {
            return;
        };
        let _ = pool.run(&mut PostgresInterrupts, |cancellation| {
            // SAFETY: plain stores to the flags a signal handler sets; the
            // main thread reads them when it polls.
            unsafe {
                pg_sys::QueryCancelPending = 1;
                pg_sys::InterruptPending = 1;
            }
            let start = Instant::now();
            while !cancellation.is_cancelled() && start.elapsed() < Duration::from_secs(10) {
                std::thread::yield_now();
            }
            SAW_CANCELLATION.store(cancellation.is_cancelled(), Ordering::Relaxed);
            Ok(())
        });
    }

    #[pg_test]
    fn a_cancel_while_a_kernel_runs_stops_it_with_query_canceled() -> Result<(), Error> {
        assert_eq!(canceled("tests.cancel_a_running_job()")?, Some(true));
        assert!(SAW_CANCELLATION.load(Ordering::Relaxed), "the job never saw the cancellation");
        Ok(())
    }
}
