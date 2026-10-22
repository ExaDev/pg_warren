//! The callbacks that give back what a backend holds of the shared cache
//! when it cannot itself (DESIGN.md §5.6, crash safety): the transaction's
//! and a subtransaction's abort, after an ERROR unwound a load, give up
//! the slots the backend was loading, so a waiter takes them over; the
//! backend's exit (`before_shmem_exit`, which runs before Postgres detaches
//! its segments) gives back its pins, claims and mappings. Registered once
//! per backend, on its first use of the shared cache.

use std::ffi::c_void;

use pgrx::pg_sys;

use super::Dsm;
use crate::cache::shared::SharedVersions;

/// Registers the callbacks, once per backend (`Dsm::open`).
pub(super) fn register() {
    // SAFETY: on the main thread, once per backend; the callbacks are
    // `extern "C-unwind"` functions that live as long as the library.
    unsafe {
        pg_sys::RegisterXactCallback(Some(transaction_end), std::ptr::null_mut());
        pg_sys::RegisterSubXactCallback(Some(subtransaction_end), std::ptr::null_mut());
        pg_sys::before_shmem_exit(Some(backend_exit), pg_sys::Datum::from(0));
    }
}

/// At a transaction's abort, gives up what it was loading.
#[pgrx::pg_guard]
unsafe extern "C-unwind" fn transaction_end(event: pg_sys::XactEvent::Type, _arg: *mut c_void) {
    if matches!(
        event,
        pg_sys::XactEvent::XACT_EVENT_ABORT | pg_sys::XactEvent::XACT_EVENT_PARALLEL_ABORT
    ) {
        Dsm::here().abort();
    }
}

/// At a subtransaction's abort (a savepoint, or a PL/pgSQL block that
/// catches the ERROR), gives up what it was loading: a load runs within
/// one call, so what is loading when a subtransaction aborts is its own.
#[pgrx::pg_guard]
unsafe extern "C-unwind" fn subtransaction_end(
    event: pg_sys::SubXactEvent::Type,
    _subtransaction: pg_sys::SubTransactionId,
    _parent: pg_sys::SubTransactionId,
    _arg: *mut c_void,
) {
    if event == pg_sys::SubXactEvent::SUBXACT_EVENT_ABORT_SUB {
        Dsm::here().abort();
    }
}

/// As the backend exits, gives back everything it holds.
#[pgrx::pg_guard]
unsafe extern "C-unwind" fn backend_exit(_code: i32, _arg: pg_sys::Datum) {
    Dsm::here().exit();
}
