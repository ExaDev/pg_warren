//! What long-running domain work reports of its progress (DESIGN.md §5.7):
//! the phase it is in, the units of it done of a total, and a few named
//! counters. Kernels report on the main thread, at the cadence they poll
//! [`Interrupts`](crate::interrupts::Interrupts), which hands them the port;
//! the adapter writes the reports where other sessions can read them.
//! Reporting never changes a result.

/// The port long-running work reports through.
pub trait Progress {
    /// Starts phase `name`, of `total` units if known, with none done.
    fn phase(&mut self, name: &'static str, total: Option<u64>);
    /// `done` units of the phase are done, never more than its total.
    fn done(&mut self, done: u64);
    /// Sets the counter `name` of the phase to `value`.
    fn detail(&mut self, name: &'static str, value: f64);
}

/// Reports nothing: for callers that do not report.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unreported;

impl Progress for Unreported {
    fn phase(&mut self, _name: &'static str, _total: Option<u64>) {}
    fn done(&mut self, _done: u64) {}
    fn detail(&mut self, _name: &'static str, _value: f64) {}
}

#[cfg(test)]
pub(crate) mod recorded {
    //! A [`Progress`] and [`Interrupts`] that keep every report, for tests
    //! of the kernels that report.

    use super::Progress;
    use crate::interrupts::{Cancelled, Interrupts};

    /// One report.
    #[derive(Debug, Clone, PartialEq)]
    pub(crate) enum Report {
        Phase(&'static str, Option<u64>),
        Done(u64),
        Detail(&'static str, f64),
    }

    /// Never interrupts, and keeps every report.
    #[derive(Debug, Default)]
    pub(crate) struct Recorded(pub(crate) Vec<Report>);

    impl Progress for Recorded {
        fn phase(&mut self, name: &'static str, total: Option<u64>) {
            self.0.push(Report::Phase(name, total));
        }
        fn done(&mut self, done: u64) {
            self.0.push(Report::Done(done));
        }
        fn detail(&mut self, name: &'static str, value: f64) {
            self.0.push(Report::Detail(name, value));
        }
    }

    impl Interrupts for Recorded {
        fn check(&mut self) -> Result<(), Cancelled> {
            Ok(())
        }
        fn progress(&mut self) -> Option<&mut dyn Progress> {
            Some(self)
        }
    }

    impl Recorded {
        /// The phases reported, in order, with their totals.
        pub(crate) fn phases(&self) -> Vec<(&'static str, Option<u64>)> {
            self.0
                .iter()
                .filter_map(|report| match report {
                    Report::Phase(name, total) => Some((*name, *total)),
                    _ => None,
                })
                .collect()
        }

        /// Asserts that within each phase `done` only grows and never
        /// exceeds the phase's total, and returns the last `done` of each.
        pub(crate) fn checked_done(&self) -> Vec<u64> {
            let mut last = Vec::new();
            let mut total = None;
            for report in &self.0 {
                match *report {
                    Report::Phase(_, phase_total) => {
                        total = phase_total;
                        last.push(0);
                    }
                    Report::Done(done) => {
                        let previous = last.last_mut().expect("done is reported within a phase");
                        assert!(done >= *previous, "done fell from {previous} to {done}");
                        assert!(total.is_none_or(|total| done <= total), "{done} of {total:?}");
                        *previous = done;
                    }
                    Report::Detail(..) => assert!(!last.is_empty(), "a detail outside a phase"),
                }
            }
            last
        }

        /// The last value reported for the counter `name`.
        pub(crate) fn last_detail(&self, name: &str) -> Option<f64> {
            self.0.iter().rev().find_map(|report| match report {
                Report::Detail(detail, value) if *detail == name => Some(*value),
                _ => None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::recorded::{Recorded, Report};
    use super::*;

    #[test]
    fn unreported_progress_is_a_no_op() {
        let mut progress = Unreported;
        progress.phase("p", Some(3));
        progress.done(1);
        progress.detail("d", 2.0);
    }

    #[test]
    fn a_recording_checks_that_done_only_grows_within_its_total() {
        let mut recorded = Recorded::default();
        recorded.phase("a", Some(3));
        recorded.done(1);
        recorded.done(3);
        recorded.detail("x", 0.5);
        recorded.phase("b", None);
        recorded.done(7);
        assert_eq!(recorded.phases(), [("a", Some(3)), ("b", None)]);
        assert_eq!(recorded.checked_done(), [3, 7]);
        assert_eq!(recorded.last_detail("x"), Some(0.5));
        assert_eq!(recorded.0[1], Report::Done(1));
    }

    #[test]
    #[should_panic(expected = "4 of Some(3)")]
    fn a_recording_refuses_done_past_its_total() {
        let mut recorded = Recorded::default();
        recorded.phase("a", Some(3));
        recorded.done(4);
        recorded.checked_done();
    }
}
