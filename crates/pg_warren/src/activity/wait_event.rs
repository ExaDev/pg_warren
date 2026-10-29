//! The wait events pg_warren's waits show in `pg_stat_activity` (DESIGN.md
//! §5.7): named, of type `Extension`, on PG 17 and 18, each registered once
//! per backend (`WaitEventExtensionNew` needs no preload); `Extension` on
//! PG 15 and 16.

use std::ffi::CStr;

use pgrx::pg_sys;

/// The wait event named `name`. On the main thread.
#[cfg(any(feature = "pg17", feature = "pg18"))]
pub(crate) fn wait_event(name: &'static CStr) -> u32 {
    use std::cell::RefCell;

    thread_local! {
        /// The events this backend has registered, by name.
        static REGISTERED: RefCell<Vec<(&'static CStr, u32)>> = const { RefCell::new(Vec::new()) };
    }
    let known = REGISTERED
        .with_borrow(|registered| registered.iter().find(|(named, _)| *named == name).copied());
    if let Some((_, event)) = known {
        return event;
    }
    // SAFETY: on the main thread, with a static name; Postgres returns the
    // same id in every backend that registers the name.
    let event = unsafe { pg_sys::WaitEventExtensionNew(name.as_ptr()) };
    REGISTERED.with_borrow_mut(|registered| registered.push((name, event)));
    event
}

/// `Extension`: PG 15 and 16 have no named extension wait events.
#[cfg(any(feature = "pg15", feature = "pg16"))]
pub(crate) fn wait_event(_name: &'static CStr) -> u32 {
    pg_sys::PG_WAIT_EXTENSION
}
