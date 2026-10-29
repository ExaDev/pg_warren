//! Compute threads across the cluster (DESIGN.md §14 item 52): a count of
//! the threads every backend's calls hold, in shared memory (the activity
//! table's header), which a call takes from before it runs on its pool and
//! gives back after. A call gets as many threads as are free under
//! `warren.cluster_threads_limit`, up to what it asks, and waits only while
//! none is. Pure Rust over an atomic, so threads stand in for backends in
//! its tests; how a call waits is the caller's (the process latch, with
//! interrupts checked, in `pool`).

use std::sync::atomic::{AtomicU32, Ordering};

/// The threads a call asking for `asked` gets while `in_use` of `cap` are
/// held: as many as are free, up to what it asks.
fn grant(asked: u32, cap: u32, in_use: u32) -> u32 {
    asked.min(cap.saturating_sub(in_use))
}

/// Takes up to `asked` threads of `cap` from the cluster's `count`: the
/// number taken, 0 while none is free.
fn take(count: &AtomicU32, asked: u32, cap: u32) -> u32 {
    let mut taken = 0;
    let _ = count.fetch_update(Ordering::AcqRel, Ordering::Acquire, |in_use| {
        taken = grant(asked, cap, in_use);
        (taken > 0).then(|| in_use.saturating_add(taken))
    });
    taken
}

/// Takes up to `asked` threads of `cap` from `count`, calling `wait`
/// while none is free: the number taken, at least one. `wait` ends the
/// wait by unwinding (an interrupt's ERROR), which takes nothing.
pub(super) fn take_waiting(count: &AtomicU32, asked: u32, cap: u32, mut wait: impl FnMut()) -> u32 {
    loop {
        let taken = take(count, asked.max(1), cap);
        if taken > 0 {
            return taken;
        }
        wait();
    }
}

/// Gives `threads` back to `count`.
pub(super) fn give_back(count: &AtomicU32, threads: u32) {
    let _ = count.fetch_update(Ordering::AcqRel, Ordering::Acquire, |in_use| {
        Some(in_use.saturating_sub(threads))
    });
}

#[cfg(test)]
mod grants {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::AtomicBool;
    use std::thread;

    use super::*;

    #[test]
    fn a_call_gets_the_threads_free_up_to_what_it_asks() {
        assert_eq!(grant(4, 8, 0), 4);
        assert_eq!(grant(4, 8, 6), 2, "two free");
        assert_eq!(grant(4, 8, 8), 0, "none free");
        assert_eq!(grant(4, 2, 5), 0, "a cap lowered below what is held");
        assert_eq!(grant(16, 4, 0), 4, "never more than the cap");
    }

    #[test]
    fn threads_taken_are_held_until_given_back() {
        let count = AtomicU32::new(0);
        assert_eq!(take(&count, 3, 4), 3);
        assert_eq!(take(&count, 3, 4), 1);
        assert_eq!(take(&count, 3, 4), 0);
        assert_eq!(count.load(Ordering::Relaxed), 4);
        give_back(&count, 3);
        assert_eq!(take(&count, 2, 4), 2);
        give_back(&count, 10);
        assert_eq!(count.load(Ordering::Relaxed), 0, "never below none");
    }

    #[test]
    fn a_call_waits_while_none_is_free() {
        let count = AtomicU32::new(2);
        let mut waits = 0;
        let taken = take_waiting(&count, 4, 2, || {
            waits += 1;
            if waits == 3 {
                give_back(&count, 1);
            }
        });
        assert_eq!((taken, waits), (1, 3));
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_wait_ended_by_an_interrupt_takes_nothing() {
        let count = AtomicU32::new(2);
        let interrupted = catch_unwind(AssertUnwindSafe(|| {
            take_waiting(&count, 1, 2, || panic!("canceling statement due to user request"))
        }));
        assert!(interrupted.is_err());
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    /// Threads stand in for backends: each takes threads, holds them a
    /// moment, and gives them back, again and again. The threads held
    /// together never exceed the cap, and every call runs.
    #[test]
    fn backends_together_never_hold_more_than_the_cap() {
        const BACKENDS: u32 = 8;
        const CALLS: u32 = 500;
        const CAP: u32 = 5;
        let count = AtomicU32::new(0);
        let held = AtomicU32::new(0);
        let over = AtomicBool::new(false);
        thread::scope(|scope| {
            for backend in 0..BACKENDS {
                let (count, held, over) = (&count, &held, &over);
                scope.spawn(move || {
                    for call in 0..CALLS {
                        let asked = 1 + (backend + call) % 4;
                        let taken = take_waiting(count, asked, CAP, thread::yield_now);
                        assert!((1..=asked).contains(&taken));
                        if held.fetch_add(taken, Ordering::AcqRel) + taken > CAP {
                            over.store(true, Ordering::Relaxed);
                        }
                        thread::yield_now();
                        held.fetch_sub(taken, Ordering::AcqRel);
                        give_back(count, taken);
                    }
                });
            }
        });
        assert!(!over.load(Ordering::Relaxed), "more than {CAP} threads held at once");
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }
}
