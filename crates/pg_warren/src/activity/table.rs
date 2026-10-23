//! The activity table (DESIGN.md §5.7, §14 item 48): a slot per possible
//! backend and the counters of backends that have exited. A backend's
//! slot is the one at its index among `MaxBackends`, so claiming it needs
//! no search; only its backend writes it, without a lock. Claiming and
//! giving back a slot change the totals of exited backends, which every
//! backend writes, so they run under the table's lock, with a change count
//! of their own: a reader that sees it unchanged around its copy of the
//! slots has every backend once, live or exited. Readers take no lock.
//!
//! Pure Rust over the table's memory, so threads stand in for backends in
//! its tests; the lock is a port (`Lock`), an LWLock in shared memory.

use super::change_count::Counted;
use super::slot::{Counters, Holder, Slot, Values, COUNTER_WORDS};

/// Serializes the backends that change the totals of exited backends.
pub(super) trait Lock {
    /// Runs `f` holding the lock exclusively.
    fn exclusive<R>(&self, f: impl FnOnce() -> R) -> R;
}

/// The table as a backend maps it.
pub(super) struct Table<'t, L> {
    pub(super) lock: L,
    /// The counters of backends that have exited.
    pub(super) exited: &'t Counted<COUNTER_WORDS>,
    pub(super) slots: &'t [Slot],
}

/// What a reader copies of the table.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Copied {
    /// The counters of backends that have exited.
    pub(crate) exited: Counters,
    /// Each claimed slot's values, by index.
    pub(crate) backends: Vec<(usize, Values)>,
}

impl Copied {
    /// Every backend's counters, live or exited.
    pub(crate) fn totals(&self) -> Counters {
        self.backends.iter().fold(self.exited, |sum, (_, values)| sum.plus(&values.counters))
    }
}

impl<L: Lock> Table<'_, L> {
    /// Claims slot `index` for `holder` and writes `values` into it; false
    /// if there is no such slot. A slot a crash left claimed was reset
    /// with the table, so whatever it holds is overwritten.
    pub(super) fn claim(&self, index: usize, holder: Holder, values: &Values) -> bool {
        let Some(slot) = self.slots.get(index) else {
            return false;
        };
        self.lock.exclusive(|| slot.write(&Values { holder, ..*values }.encode()));
        true
    }

    /// Writes `values` into slot `index`, which this backend holds.
    pub(super) fn write(&self, index: usize, values: &Values) {
        if let Some(slot) = self.slots.get(index) {
            slot.write(&values.encode());
        }
    }

    /// Gives back slot `index`, whose backend exits with `counters`: they
    /// join the totals of exited backends as the slot is freed, in one
    /// change of the totals.
    pub(super) fn release(&self, index: usize, counters: &Counters) {
        let Some(slot) = self.slots.get(index) else {
            return;
        };
        self.lock.exclusive(|| {
            self.exited.begin();
            self.exited.add(&counters.encode());
            slot.write(&Values::FREE.encode());
            self.exited.end();
        });
    }

    /// A copy of the table: the totals of exited backends and every claimed
    /// slot, retried until no backend exited or was claimed while it was
    /// made.
    pub(super) fn copy(&self) -> Copied {
        loop {
            let before = self.exited.changes();
            if before.is_multiple_of(2) {
                let exited = Counters::decode(&self.exited.read());
                let backends = self
                    .slots
                    .iter()
                    .enumerate()
                    .map(|(index, slot)| (index, Values::decode(&slot.read())))
                    .filter(|(_, values)| values.holder.pid != 0)
                    .collect();
                if self.exited.changes() == before {
                    return Copied { exited, backends };
                }
            }
            std::hint::spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::thread;

    use pgrx::pg_sys::Oid;

    use super::*;
    use crate::activity::slot::Refused;

    impl Lock for &Mutex<()> {
        fn exclusive<R>(&self, f: impl FnOnce() -> R) -> R {
            let _held = self.lock().unwrap();
            f()
        }
    }

    fn holder(pid: i32) -> Holder {
        Holder { pid, database: Oid::from(1), role: Oid::from(2), started: 3 }
    }

    fn counted(lookups: u64) -> Values {
        let counters = Counters { lookups, ..Values::FREE.counters };
        Values { counters, ..Values::FREE }
    }

    struct Memory {
        lock: Mutex<()>,
        exited: Counted<COUNTER_WORDS>,
        slots: Vec<Slot>,
    }

    impl Memory {
        fn new(slots: usize) -> Self {
            Self {
                lock: Mutex::new(()),
                exited: Counted::zero(),
                slots: (0..slots).map(|_| Slot::zero()).collect(),
            }
        }

        fn table(&self) -> Table<'_, &Mutex<()>> {
            Table { lock: &self.lock, exited: &self.exited, slots: &self.slots }
        }
    }

    #[test]
    fn a_claimed_slot_is_listed_with_its_holder_until_it_is_given_back() {
        let memory = Memory::new(4);
        let table = memory.table();
        assert!(table.claim(2, holder(42), &counted(5)));
        assert!(!table.claim(4, holder(43), &counted(0)), "past MaxBackends");
        let copied = table.copy();
        assert_eq!(copied.backends.len(), 1);
        assert_eq!(copied.backends[0].0, 2);
        assert_eq!(copied.backends[0].1.holder, holder(42));
        assert_eq!(copied.totals().lookups, 5);
        table.release(2, &counted(7).counters);
        let copied = table.copy();
        assert!(copied.backends.is_empty());
        assert_eq!(copied.exited.lookups, 7, "the counters it exited with");
    }

    #[test]
    fn a_slot_given_back_is_claimed_afresh() {
        let memory = Memory::new(1);
        let table = memory.table();
        table.claim(0, holder(1), &counted(3));
        table.release(0, &counted(3).counters);
        table.claim(0, holder(2), &counted(0));
        let copied = table.copy();
        assert_eq!(copied.backends[0].1.holder.pid, 2);
        assert_eq!(copied.backends[0].1.counters.lookups, 0, "a new backend counts from 0");
        assert_eq!(copied.totals().lookups, 3);
    }

    #[test]
    fn exits_add_up_in_the_totals() {
        let memory = Memory::new(3);
        let table = memory.table();
        let mut refused = counted(1).counters;
        refused.refuse(Refused::NoRoom);
        for index in 0..3 {
            table.claim(index, holder(10 + index as i32), &counted(0));
            table.release(index, &refused);
        }
        let exited = table.copy().exited;
        assert_eq!((exited.lookups, exited.refused(Refused::NoRoom)), (3, 3));
    }

    /// Threads stand in for backends: each claims its own slot, counts,
    /// and exits, again and again, while a reader copies the table. Each
    /// copy counts every backend once, live or exited, so the totals only
    /// grow, and no slot is ever seen with another's counts.
    #[test]
    fn a_reader_counts_each_backend_once_while_backends_come_and_go() {
        const BACKENDS: usize = 4;
        const LIVES: u64 = 300;
        let memory = Memory::new(BACKENDS);
        let exited = AtomicUsize::new(0);
        thread::scope(|scope| {
            for index in 0..BACKENDS {
                let (table, exited) = (memory.table(), &exited);
                scope.spawn(move || {
                    for life in 0..LIVES {
                        let pid = (index as i32 + 1) * 1000 + life as i32;
                        table.claim(index, holder(pid), &counted(0));
                        for lookups in 1..=3 {
                            let values = Values { holder: holder(pid), ..counted(lookups) };
                            table.write(index, &values);
                        }
                        table.release(index, &counted(3).counters);
                    }
                    exited.fetch_add(1, Ordering::Release);
                });
            }
            let (table, exited) = (memory.table(), &exited);
            scope.spawn(move || {
                let mut last = 0;
                while exited.load(Ordering::Acquire) < BACKENDS {
                    let copied = table.copy();
                    for (index, values) in &copied.backends {
                        assert_eq!(values.holder.pid / 1000, *index as i32 + 1, "another's slot");
                    }
                    let total = copied.totals().lookups;
                    assert!(total >= last, "the totals went back from {last} to {total}");
                    last = total;
                }
            });
        });
        let copied = memory.table().copy();
        assert!(copied.backends.is_empty());
        assert_eq!(copied.exited.lookups, BACKENDS as u64 * LIVES * 3);
    }
}
