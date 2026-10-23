//! The progress of a job on the compute pool (DESIGN.md §5.7). Only the
//! thread that waits for the pool may report, so the job counts into a
//! shared [`Tally`] and that thread reports it each time it polls its
//! interrupts ([`Reporting`]), every millisecond while the job runs.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::interrupts::{Cancelled, Interrupts};
use crate::progress::Progress;

/// What a job has done, written by the job and read by the waiting thread.
pub(crate) struct Tally<const N: usize> {
    /// The phase's total, `UNKNOWN` until the job knows it.
    total: AtomicU64,
    done: AtomicU64,
    /// The phase's counters, as `f64` bits.
    details: [AtomicU64; N],
}

const UNKNOWN: u64 = u64::MAX;

impl<const N: usize> Tally<N> {
    /// A tally of a phase of `total` units, if known, with none done.
    pub(crate) fn new(total: Option<u64>) -> Self {
        Self {
            total: AtomicU64::new(total.unwrap_or(UNKNOWN)),
            done: AtomicU64::new(0),
            details: [const { AtomicU64::new(0) }; N],
        }
    }

    /// The phase has `total` units; set before any is done.
    pub(crate) fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Release);
    }

    /// `done` units are done.
    pub(crate) fn set_done(&self, done: u64) {
        self.done.store(done, Ordering::Relaxed);
    }

    /// `units` more units are done.
    pub(crate) fn add_done(&self, units: u64) {
        self.done.fetch_add(units, Ordering::Relaxed);
    }

    /// Sets the counter at `at` to `value`.
    pub(crate) fn set_detail(&self, at: usize, value: f64) {
        self.details[at].store(value.to_bits(), Ordering::Relaxed);
    }

    /// The total, if known. Read before `done`, so that `done` is never
    /// reported over it.
    fn total(&self) -> Option<u64> {
        Some(self.total.load(Ordering::Acquire)).filter(|&total| total != UNKNOWN)
    }
}

/// Interrupts that report a [`Tally`] as phase `phase` of the progress of
/// the interrupts they wrap, each time they are checked.
pub(crate) struct Reporting<'a, I: Interrupts + ?Sized, const N: usize> {
    interrupts: &'a mut I,
    tally: &'a Tally<N>,
    phase: &'static str,
    names: [&'static str; N],
    total: Option<u64>,
}

impl<'a, I: Interrupts + ?Sized, const N: usize> Reporting<'a, I, N> {
    /// Starts `phase` and reports `tally` under it, its counters named
    /// `names`.
    pub(crate) fn start(
        interrupts: &'a mut I,
        tally: &'a Tally<N>,
        phase: &'static str,
        names: [&'static str; N],
    ) -> Self {
        let total = tally.total();
        if let Some(progress) = interrupts.progress() {
            progress.phase(phase, total);
        }
        Self { interrupts, tally, phase, names, total }
    }

    /// Reports the tally now: a total that has become known restarts the
    /// phase with it.
    pub(crate) fn report(&mut self) {
        let total = self.tally.total();
        let done = self.tally.done.load(Ordering::Relaxed);
        let Some(progress) = self.interrupts.progress() else { return };
        if total != self.total {
            progress.phase(self.phase, total);
            self.total = total;
        }
        progress.done(done);
        for (name, value) in self.names.iter().zip(&self.tally.details) {
            progress.detail(name, f64::from_bits(value.load(Ordering::Relaxed)));
        }
    }
}

impl<I: Interrupts + ?Sized, const N: usize> Interrupts for Reporting<'_, I, N> {
    fn check(&mut self) -> Result<(), Cancelled> {
        self.report();
        self.interrupts.check()
    }

    fn progress(&mut self) -> Option<&mut dyn Progress> {
        self.interrupts.progress()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interrupts::Uninterrupted;
    use crate::progress::recorded::{Recorded, Report};

    #[test]
    fn each_check_reports_the_tally_under_the_phase() {
        let mut recorded = Recorded::default();
        let tally = Tally::<1>::new(Some(10));
        let mut reporting = Reporting::start(&mut recorded, &tally, "moving", ["moved"]);
        tally.set_done(3);
        tally.set_detail(0, 42.0);
        reporting.check().unwrap();
        tally.add_done(2);
        reporting.report();
        assert_eq!(
            recorded.0,
            [
                Report::Phase("moving", Some(10)),
                Report::Done(3),
                Report::Detail("moved", 42.0),
                Report::Done(5),
                Report::Detail("moved", 42.0),
            ]
        );
    }

    #[test]
    fn a_total_learned_by_the_job_restarts_the_phase_with_it() {
        let mut recorded = Recorded::default();
        let tally = Tally::<0>::new(None);
        let mut reporting = Reporting::start(&mut recorded, &tally, "refining", []);
        reporting.report();
        tally.set_total(4);
        tally.add_done(1);
        reporting.report();
        reporting.report();
        assert_eq!(recorded.phases(), [("refining", None), ("refining", Some(4))]);
        assert_eq!(recorded.checked_done(), [0, 1]);
    }

    #[test]
    fn checks_pass_through_and_nothing_is_reported_without_a_port() {
        struct Stop;
        impl Interrupts for Stop {
            fn check(&mut self) -> Result<(), Cancelled> {
                Err(Cancelled)
            }
        }
        let tally = Tally::<0>::new(None);
        assert_eq!(Reporting::start(&mut Stop, &tally, "p", []).check(), Err(Cancelled));
        assert_eq!(Reporting::start(&mut Uninterrupted, &tally, "p", []).check(), Ok(()));
    }

    #[test]
    fn a_job_on_another_thread_is_reported_without_done_passing_its_total() {
        let mut recorded = Recorded::default();
        let tally = Tally::<0>::new(None);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                tally.set_total(1000);
                for _ in 0..1000 {
                    tally.add_done(1);
                }
            });
            let mut reporting = Reporting::start(&mut recorded, &tally, "p", []);
            for _ in 0..200 {
                reporting.report();
            }
        });
        recorded.checked_done();
    }
}
