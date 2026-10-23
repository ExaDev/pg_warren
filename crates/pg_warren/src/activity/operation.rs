//! The operation frame's writers (DESIGN.md §5.7): a native call opens its
//! operation with [`Running::start`] and reports its phases, units done and
//! counters through [`phase`], [`done`] and [`detail`], directly or through
//! `warren-core`'s progress port. Dropping the guard, at the call's end or
//! as an ERROR unwinds it, restores the operation it interrupted (a load
//! inside `eigen`, say). The writers are pure Rust over this backend's own
//! slot, and do nothing while no operation is open.

use pgrx::pg_sys;

use super::backend::{claim, record};
use super::frame::{Frame, Name, Operation};

/// The open operation, until dropped.
#[must_use = "the operation ends when the guard is dropped"]
pub(crate) struct Running {
    /// The operation it interrupted.
    interrupted: Operation,
}

impl Running {
    /// Opens operation `name`, on `graph` and `version` if known, as of
    /// now. On the main thread, inside a call.
    pub(crate) fn start(name: &str, graph: Option<i32>, version: Option<i64>) -> Self {
        claim();
        Self::open(Frame { name: Name::new(name), graph, version, start: now(), ..Frame::NONE })
    }

    /// Opens operation `name` without asking Postgres who or when, for
    /// plain tests of what reports to it.
    #[cfg(test)]
    pub(crate) fn opened(name: &str) -> Self {
        Self::open(Frame { name: Name::new(name), ..Frame::NONE })
    }

    /// Opens the operation of `frame`.
    fn open(frame: Frame) -> Self {
        let mut interrupted = Operation::NONE;
        record(|values| {
            interrupted =
                std::mem::replace(&mut values.operation, Operation { frame, ..Operation::NONE });
        });
        Self { interrupted }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let interrupted = self.interrupted;
        record(|values| values.operation = interrupted);
    }
}

/// The current time, as a `timestamptz`.
pub(super) fn now() -> i64 {
    // SAFETY: reads the clock; callable from any backend code.
    unsafe { pg_sys::GetCurrentTimestamp() }
}

/// Changes the open operation, if there is one.
fn change(change: impl FnOnce(&mut Operation)) {
    record(|values| {
        if values.operation.frame.is_some() {
            change(&mut values.operation);
        }
    });
}

/// The open operation is on `version`, once known.
pub(crate) fn version(version: i64) {
    change(|operation| operation.frame.version = Some(version));
}

/// Starts phase `name` of the open operation, of `total` units if known.
pub(crate) fn phase(name: &str, total: Option<u64>) {
    change(|operation| operation.enter(name, total));
}

/// `units` of the phase are done. A total that was an estimate (the
/// planner's rows) is raised to them, so `done` never exceeds `total`.
pub(crate) fn done(units: u64) {
    change(|operation| {
        operation.frame.done = Some(units);
        operation.frame.total = operation.frame.total.map(|total| total.max(units));
    });
}

/// Sets the open operation's counter `name` to `value`.
pub(crate) fn detail(name: &str, value: f64) {
    change(|operation| operation.set_detail(name, value));
}

/// Units done one at a time (rows read), reported every [`UNITS_PER_REPORT`],
/// the interrupt poll's cadence, and when finished.
pub(crate) struct Units(u64);

/// How many units a report covers.
const UNITS_PER_REPORT: u64 = 65_536;

impl Units {
    /// None done yet.
    pub(crate) const fn new() -> Self {
        Self(0)
    }

    /// One more unit is done.
    pub(crate) fn one(&mut self) {
        self.0 += 1;
        if self.0.is_multiple_of(UNITS_PER_REPORT) {
            done(self.0);
        }
    }

    /// Reports the units done.
    pub(crate) fn finish(&self) {
        done(self.0);
    }
}
