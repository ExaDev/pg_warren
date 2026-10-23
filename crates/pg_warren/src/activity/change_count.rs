//! Words in shared memory that one writer changes and any process reads
//! without a lock, with the protocol Postgres uses for `PgBackendStatus`
//! (`st_changecount`, DESIGN.md §5.7): the writer makes a count odd before
//! it writes and even after, with barriers between; a reader copies the
//! words and retries until it reads the same even count before and after.
//! Every word is an atomic, so a torn copy is a retry, never a data race.

use std::sync::atomic::{fence, AtomicU64, Ordering};

/// `N` words and their change count, as they lie in shared memory.
#[repr(C)]
pub(super) struct Counted<const N: usize> {
    changes: AtomicU64,
    words: [AtomicU64; N],
}

impl<const N: usize> Counted<N> {
    /// Zeroed words, never written.
    pub(super) const fn zero() -> Self {
        Self { changes: AtomicU64::new(0), words: [const { AtomicU64::new(0) }; N] }
    }

    /// Writes `words`. Only one process may write a given `Counted` at a
    /// time (its owner, or the holder of a lock its writers share).
    pub(super) fn write(&self, words: &[u64; N]) {
        self.begin();
        for (word, &value) in self.words.iter().zip(words) {
            word.store(value, Ordering::Relaxed);
        }
        self.end();
    }

    /// Makes the count odd: a write has begun.
    pub(super) fn begin(&self) {
        self.changes.fetch_add(1, Ordering::Relaxed);
        fence(Ordering::Release);
    }

    /// Makes the count even again: the write has ended.
    pub(super) fn end(&self) {
        self.changes.fetch_add(1, Ordering::Release);
    }

    /// Adds `words` to the words: a write between [`begin`](Self::begin)
    /// and [`end`](Self::end), by their one writer.
    pub(super) fn add(&self, words: &[u64; N]) {
        for (word, &value) in self.words.iter().zip(words) {
            let sum = word.load(Ordering::Relaxed).saturating_add(value);
            word.store(sum, Ordering::Relaxed);
        }
    }

    /// The count now: even when no write is under way.
    pub(super) fn changes(&self) -> u64 {
        self.changes.load(Ordering::Acquire)
    }

    /// A copy of the words that no write tore.
    pub(super) fn read(&self) -> [u64; N] {
        loop {
            if let Some(words) = self.try_read() {
                return words;
            }
            std::hint::spin_loop();
        }
    }

    /// A copy of the words, if no write was under way while it was made.
    fn try_read(&self) -> Option<[u64; N]> {
        let before = self.changes();
        let mut words = [0; N];
        for (copy, word) in words.iter_mut().zip(&self.words) {
            *copy = word.load(Ordering::Relaxed);
        }
        fence(Ordering::Acquire);
        let after = self.changes.load(Ordering::Relaxed);
        (before == after && before.is_multiple_of(2)).then_some(words)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::thread;

    use super::*;

    #[test]
    fn a_write_is_read_back_whole() {
        let counted = Counted::<3>::zero();
        assert_eq!(counted.read(), [0, 0, 0]);
        counted.write(&[1, 2, 3]);
        assert_eq!(counted.read(), [1, 2, 3]);
        assert_eq!(counted.changes(), 2);
    }

    #[test]
    fn a_write_under_way_is_not_read() {
        let counted = Counted::<2>::zero();
        counted.begin();
        counted.add(&[5, 6]);
        assert_eq!(counted.try_read(), None, "the count is odd");
        counted.end();
        assert_eq!(counted.try_read(), Some([5, 6]));
    }

    #[test]
    fn additions_saturate() {
        let counted = Counted::<1>::zero();
        counted.write(&[u64::MAX - 1]);
        counted.begin();
        counted.add(&[5]);
        counted.end();
        assert_eq!(counted.read(), [u64::MAX]);
    }

    /// A writer thread stands in for a backend writing its slot, a reader
    /// for another backend reading it: every copy is one the writer wrote,
    /// all of its words from the same write.
    #[test]
    fn a_reader_never_sees_a_torn_write() {
        let counted = Counted::<8>::zero();
        let done = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                for value in 1..=20_000u64 {
                    counted.write(&[value; 8]);
                }
                done.store(true, Ordering::Release);
            });
            let mut last = 0;
            while !done.load(Ordering::Acquire) {
                let words = counted.read();
                assert!(words.iter().all(|&word| word == words[0]), "torn: {words:?}");
                assert!(words[0] >= last, "went back from {last} to {}", words[0]);
                last = words[0];
            }
        });
        assert_eq!(counted.read(), [20_000; 8]);
    }
}
