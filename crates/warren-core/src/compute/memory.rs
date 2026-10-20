//! The compute memory budget: every vector, message buffer and work buffer
//! is charged to it, and a charge past its limit is an error rather than an
//! out-of-memory abort (DESIGN.md §6.9, `warren.max_compute_memory`).
//!
//! A budget may count memory held elsewhere (by a cache, say), and part of
//! that may be reclaimable: a [`Reclaim`] hook frees it when a charge would
//! otherwise fail. The hook runs only on the thread that made the budget,
//! since what it frees may be reachable only from there (a Postgres
//! backend's main thread, §6.9); a charge on another thread is refused as
//! if there were no hook, so a caller about to fan work out reclaims ahead
//! with [`ComputeMemory::reclaim_for`].

use std::fmt::Debug;
use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, ThreadId};

use super::message::{growth, reserve};
use super::ComputeError;

/// Frees memory a budget counts as held, when a charge needs the room.
pub trait Reclaim: Debug + Send + Sync {
    /// Frees at least `bytes` of the held memory if it can, and returns the
    /// bytes it freed (which may be fewer, or more).
    fn reclaim(&self, bytes: u64) -> u64;
}

/// A budget of compute memory in bytes. Clones share the budget.
#[derive(Debug, Clone)]
pub struct ComputeMemory(Arc<Budget>);

#[derive(Debug)]
struct Budget {
    limit: u64,
    in_use: AtomicU64,
    /// The held bytes the hook may still give back.
    reclaimable: AtomicU64,
    /// The hook, and the thread it may run on.
    reclaim: Option<(ThreadId, Box<dyn Reclaim>)>,
}

impl ComputeMemory {
    /// A budget of `limit` bytes, none in use.
    pub fn new(limit: u64) -> Self {
        Self::with_held(limit, 0)
    }

    /// A budget of `limit` bytes of which `held` are in use from the start
    /// and for as long as the budget lives: memory held elsewhere (by a
    /// cache, say) that the budget's charges must fit beside.
    pub fn with_held(limit: u64, held: u64) -> Self {
        Self::budget(limit, held, 0, None)
    }

    /// A budget of `limit` bytes of which `held` are held elsewhere, as
    /// [`Self::with_held`], and up to `reclaimable` of those (at most
    /// `held`) can be freed by `reclaim`. A charge that would not fit
    /// calls it, on the calling thread only, for the bytes it is short,
    /// releases what it freed (no more than is still reclaimable) and is
    /// tried again; it is not called if freeing all the reclaimable bytes
    /// would still leave the charge short.
    pub fn reclaiming(limit: u64, held: u64, reclaimable: u64, reclaim: Box<dyn Reclaim>) -> Self {
        let owner = thread::current().id();
        Self::budget(limit, held, reclaimable.min(held), Some((owner, reclaim)))
    }

    fn budget(
        limit: u64,
        held: u64,
        reclaimable: u64,
        reclaim: Option<(ThreadId, Box<dyn Reclaim>)>,
    ) -> Self {
        Self(Arc::new(Budget {
            limit,
            in_use: AtomicU64::new(held),
            reclaimable: AtomicU64::new(reclaimable),
            reclaim,
        }))
    }

    /// The budget's limit in bytes.
    pub fn limit(&self) -> u64 {
        self.0.limit
    }

    /// The bytes currently charged.
    pub fn in_use(&self) -> u64 {
        self.0.in_use.load(Ordering::Relaxed)
    }

    /// Charges `bytes` to the budget until the returned charge is dropped.
    pub fn charge(&self, bytes: u64) -> Result<MemoryCharge, ComputeError> {
        self.reserve(bytes)?;
        Ok(MemoryCharge { memory: self.clone(), bytes })
    }

    /// Reclaims held memory, as a failing charge would, until `bytes` more
    /// would fit: for charges that will be made on other threads, where
    /// nothing is reclaimed. Does nothing off the budget's thread, or if
    /// they fit already.
    pub fn reclaim_for(&self, bytes: u64) {
        self.reclaim(self.in_use().saturating_add(bytes).saturating_sub(self.0.limit));
    }

    /// Adds `bytes` to the bytes in use, if that stays within the limit,
    /// reclaiming held memory if it would not.
    fn reserve(&self, bytes: u64) -> Result<(), ComputeError> {
        match self.try_reserve(bytes) {
            Err(ComputeError::MemoryLimit { in_use, .. }) => {
                let short = in_use.saturating_add(bytes).saturating_sub(self.0.limit);
                self.reclaim(short);
                self.try_reserve(bytes)
            }
            result => result,
        }
    }

    /// Calls the hook for `short` bytes, on the budget's thread and if the
    /// reclaimable bytes cover them, and releases what it freed.
    fn reclaim(&self, short: u64) {
        let budget = &self.0;
        let Some((owner, reclaim)) = &budget.reclaim else {
            return;
        };
        let reclaimable = budget.reclaimable.load(Ordering::Relaxed);
        if short == 0 || short > reclaimable || thread::current().id() != *owner {
            return;
        }
        let released = reclaim.reclaim(short).min(reclaimable);
        budget.reclaimable.fetch_sub(released, Ordering::Relaxed);
        self.release(released);
    }

    /// Adds `bytes` to the bytes in use, if that stays within the limit.
    fn try_reserve(&self, bytes: u64) -> Result<(), ComputeError> {
        let budget = &self.0;
        let mut in_use = budget.in_use.load(Ordering::Relaxed);
        loop {
            let total = in_use.checked_add(bytes).filter(|&total| total <= budget.limit).ok_or(
                ComputeError::MemoryLimit { requested: bytes, in_use, limit: budget.limit },
            )?;
            match budget.in_use.compare_exchange_weak(
                in_use,
                total,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => in_use = current,
            }
        }
        Ok(())
    }

    fn release(&self, bytes: u64) {
        self.0.in_use.fetch_sub(bytes, Ordering::Relaxed);
    }
}

impl ComputeMemory {
    /// Charges the bytes of `len` elements of `E`.
    pub(crate) fn charge_elements<E>(&self, len: usize) -> Result<MemoryCharge, ComputeError> {
        let bytes = (len as u64).saturating_mul(size_of::<E>() as u64);
        self.charge(bytes)
    }

    /// `len` copies of `value` in a buffer reserved with `try_reserve` and
    /// charged to the budget before it is allocated.
    pub(crate) fn filled<E: Clone>(
        &self,
        len: usize,
        value: E,
    ) -> Result<(Vec<E>, MemoryCharge), ComputeError> {
        let charge = self.charge_elements::<E>(len)?;
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(len)
            .map_err(|_| ComputeError::OutOfMemory { bytes: charge.bytes() })?;
        buffer.resize(len, value);
        Ok((buffer, charge))
    }
}

/// Bytes charged to a [`ComputeMemory`], released when dropped.
#[derive(Debug)]
pub struct MemoryCharge {
    memory: ComputeMemory,
    bytes: u64,
}

impl MemoryCharge {
    /// The bytes this charge holds.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl MemoryCharge {
    /// Charges `bytes` more to the same budget.
    pub(crate) fn grow(&mut self, bytes: u64) -> Result<(), ComputeError> {
        self.memory.reserve(bytes)?;
        self.bytes += bytes;
        Ok(())
    }

    /// Room for one more element in `buffer`: the capacity it grows by is
    /// charged first and then reserved exactly, so the charge follows the
    /// reserved capacity; nothing is charged if either fails.
    pub(crate) fn reserve_one<T>(&mut self, buffer: &mut Vec<T>) -> Result<(), ComputeError> {
        let bytes = growth(buffer, 1);
        self.grow(bytes)?;
        reserve(buffer, 1).map_err(|_| {
            self.shrink(bytes);
            ComputeError::OutOfMemory { bytes }
        })
    }

    /// Releases `bytes` of this charge.
    pub(crate) fn shrink(&mut self, bytes: u64) {
        let bytes = bytes.min(self.bytes);
        self.memory.release(bytes);
        self.bytes -= bytes;
    }
}

/// The smallest and largest block an [`Allowance`] reserves ahead.
const BLOCK: (u64, u64) = (1 << 12, 1 << 20);

/// A charge for a buffer that grows by small steps, as a worker thread
/// builds its part of a result: it reserves the budget ahead in blocks
/// (growing with what it holds, up to 1 MiB), so the shared budget is
/// touched rarely, and falls back to the exact bytes near the limit.
/// [`Self::settle`] returns what it reserved but did not use.
#[derive(Debug)]
pub(crate) struct Allowance {
    charge: MemoryCharge,
    used: u64,
}

impl Allowance {
    /// Nothing used or reserved yet.
    pub(crate) fn new(memory: &ComputeMemory) -> Result<Self, ComputeError> {
        Ok(Self { charge: memory.charge(0)?, used: 0 })
    }

    /// The bytes taken.
    pub(crate) fn used(&self) -> u64 {
        self.used
    }

    /// Takes `bytes` more, reserving ahead if the reserve runs out.
    pub(crate) fn take(&mut self, bytes: u64) -> Result<(), ComputeError> {
        let used = self.used.saturating_add(bytes);
        let reserved = self.charge.bytes();
        if used > reserved {
            let short = used - reserved;
            let ahead = reserved.clamp(BLOCK.0, BLOCK.1).max(short);
            if self.charge.grow(ahead).is_err() {
                self.charge.grow(short)?;
            }
        }
        self.used = used;
        Ok(())
    }

    /// Room for one more element in `buffer`: the capacity it grows by is
    /// taken first and then reserved exactly.
    pub(crate) fn reserve_one<T>(&mut self, buffer: &mut Vec<T>) -> Result<(), ComputeError> {
        let bytes = growth(buffer, 1);
        self.take(bytes)?;
        reserve(buffer, 1).map_err(|_| {
            self.used -= bytes;
            ComputeError::OutOfMemory { bytes }
        })
    }

    /// Releases the bytes reserved ahead.
    pub(crate) fn settle(&mut self) {
        self.charge.shrink(self.charge.bytes() - self.used);
    }
}

impl Drop for MemoryCharge {
    fn drop(&mut self) {
        self.memory.release(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::thread;

    use super::*;

    #[test]
    fn a_charge_holds_its_bytes_until_dropped() {
        let memory = ComputeMemory::new(100);
        let charge = memory.charge(60).unwrap();
        assert_eq!(charge.bytes(), 60);
        assert_eq!(memory.in_use(), 60);
        drop(charge);
        assert_eq!(memory.in_use(), 0);
    }

    #[test]
    fn a_charge_past_the_limit_is_an_error() {
        let memory = ComputeMemory::new(100);
        let _held = memory.charge(60).unwrap();
        assert_eq!(
            memory.charge(41).unwrap_err(),
            ComputeError::MemoryLimit { requested: 41, in_use: 60, limit: 100 }
        );
        assert!(memory.charge(40).is_ok());
    }

    #[test]
    fn bytes_held_elsewhere_count_against_the_limit_for_the_budgets_life() {
        let memory = ComputeMemory::with_held(100, 30);
        assert_eq!((memory.limit(), memory.in_use()), (100, 30));
        assert_eq!(
            memory.charge(71).unwrap_err(),
            ComputeError::MemoryLimit { requested: 71, in_use: 30, limit: 100 }
        );
        drop(memory.charge(70).unwrap());
        assert_eq!(memory.in_use(), 30, "the held bytes are never released");
        let over = ComputeMemory::with_held(100, 130);
        assert!(over.charge(0).is_err(), "nothing fits once they exceed the limit");
    }

    /// A reclaim hook that frees `frees` bytes per call and records each
    /// call's request.
    #[derive(Debug, Clone)]
    struct Spare {
        frees: u64,
        requests: Arc<Mutex<Vec<u64>>>,
    }

    impl Spare {
        fn new(frees: u64) -> Self {
            Self { frees, requests: Arc::default() }
        }

        fn requests(&self) -> Vec<u64> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Reclaim for Spare {
        fn reclaim(&self, bytes: u64) -> u64 {
            self.requests.lock().unwrap().push(bytes);
            self.frees
        }
    }

    #[test]
    fn a_charge_that_fails_reclaims_held_bytes_then_succeeds() {
        let spare = Spare::new(25);
        let memory = ComputeMemory::reclaiming(100, 50, 40, Box::new(spare.clone()));
        let _fits = memory.charge(30).unwrap();
        assert!(spare.requests().is_empty(), "nothing is reclaimed while charges fit");
        let charge = memory.charge(40).unwrap();
        assert_eq!(spare.requests(), [20], "asked for what the charge was short");
        assert_eq!(memory.in_use(), 50 - 25 + 30 + 40);
        drop(charge);
        assert_eq!(memory.in_use(), 25 + 30, "reclaimed held bytes stay released");
    }

    #[test]
    fn no_more_is_released_than_was_reclaimable() {
        let spare = Spare::new(1000);
        let memory = ComputeMemory::reclaiming(100, 80, 30, Box::new(spare.clone()));
        let _charge = memory.charge(40).unwrap();
        assert_eq!(memory.in_use(), 80 - 30 + 40);
        assert_eq!(
            memory.charge(11).unwrap_err(),
            ComputeError::MemoryLimit { requested: 11, in_use: 90, limit: 100 }
        );
        assert_eq!(spare.requests(), [20], "nothing is left to reclaim");
    }

    #[test]
    fn a_charge_that_reclaiming_could_not_fit_reclaims_nothing() {
        let spare = Spare::new(30);
        let memory = ComputeMemory::reclaiming(100, 50, 30, Box::new(spare.clone()));
        assert!(matches!(memory.charge(81), Err(ComputeError::MemoryLimit { .. })));
        assert!(spare.requests().is_empty(), "freeing all 30 would leave it 1 short");
        assert_eq!(memory.in_use(), 50);
    }

    #[test]
    fn a_hook_that_frees_too_little_leaves_the_charge_refused() {
        let spare = Spare::new(5);
        let memory = ComputeMemory::reclaiming(100, 50, 30, Box::new(spare.clone()));
        assert!(matches!(memory.charge(60), Err(ComputeError::MemoryLimit { .. })));
        assert_eq!((spare.requests(), memory.in_use()), (vec![10], 45));
    }

    #[test]
    fn only_the_thread_that_made_the_budget_reclaims() {
        let spare = Spare::new(30);
        let memory = ComputeMemory::reclaiming(100, 50, 30, Box::new(spare.clone()));
        let worker = memory.clone();
        let refused =
            thread::spawn(move || worker.charge(60).map(|charge| charge.bytes())).join().unwrap();
        assert!(matches!(refused, Err(ComputeError::MemoryLimit { .. })));
        assert!(spare.requests().is_empty(), "off its thread the hook is never called");
        assert_eq!(memory.charge(60).unwrap().bytes(), 60);
        assert_eq!(spare.requests(), [10]);
    }

    #[test]
    fn room_is_reclaimed_ahead_for_charges_off_the_thread() {
        let spare = Spare::new(30);
        let memory = ComputeMemory::reclaiming(100, 50, 30, Box::new(spare.clone()));
        memory.reclaim_for(10);
        assert!(spare.requests().is_empty(), "10 more already fits");
        memory.reclaim_for(70);
        assert_eq!((spare.requests(), memory.in_use()), (vec![20], 20));
        let worker = memory.clone();
        let charged =
            thread::spawn(move || worker.charge(70).map(|charge| charge.bytes())).join().unwrap();
        assert_eq!(charged, Ok(70));
    }

    #[test]
    fn clones_share_the_budget() {
        let memory = ComputeMemory::new(10);
        let _held = memory.clone().charge(10).unwrap();
        assert_eq!(memory.in_use(), 10);
        assert!(memory.charge(1).is_err());
    }

    #[test]
    fn a_charge_grows_within_the_limit_and_shrinks() {
        let memory = ComputeMemory::new(10);
        let mut charge = memory.charge(4).unwrap();
        charge.grow(6).unwrap();
        assert!(matches!(charge.grow(1), Err(ComputeError::MemoryLimit { .. })));
        assert_eq!((charge.bytes(), memory.in_use()), (10, 10));
        charge.shrink(7);
        assert_eq!((charge.bytes(), memory.in_use()), (3, 3));
        charge.shrink(99);
        assert_eq!((charge.bytes(), memory.in_use()), (0, 0));
    }

    #[test]
    fn room_for_one_more_element_is_charged_first() {
        let memory = ComputeMemory::new(12);
        let mut charge = memory.charge(0).unwrap();
        let mut buffer = Vec::<u64>::new();
        charge.reserve_one(&mut buffer).unwrap();
        assert_eq!((buffer.capacity(), charge.bytes()), (1, 8));
        buffer.push(0);
        // Doubling to two elements would charge 8 more.
        assert!(matches!(charge.reserve_one(&mut buffer), Err(ComputeError::MemoryLimit { .. })));
        assert_eq!(memory.in_use(), 8);
    }

    #[test]
    fn an_allowance_reserves_ahead_in_blocks_and_settles_to_what_it_used() {
        let memory = ComputeMemory::new(1 << 20);
        let mut allowance = Allowance::new(&memory).unwrap();
        allowance.take(8).unwrap();
        let reserved = memory.in_use();
        assert!(reserved >= 4096);
        let mut buffer = Vec::<u64>::new();
        for _ in 0..10 {
            allowance.reserve_one(&mut buffer).unwrap();
            buffer.push(0);
        }
        // 8 bytes, then the buffer's capacity: 1, 2, 4, 8, then 16 elements.
        let used = 8 + 16 * 8;
        assert_eq!(buffer.capacity(), 16);
        assert_eq!((allowance.used(), memory.in_use()), (used, reserved));
        allowance.settle();
        assert_eq!((allowance.used(), memory.in_use()), (used, used));
        allowance.take(1 << 16).unwrap();
        assert_eq!(memory.in_use(), used + (1 << 16));
        drop(allowance);
        assert_eq!(memory.in_use(), 0);
    }

    #[test]
    fn an_allowance_takes_the_exact_bytes_near_the_limit() {
        let memory = ComputeMemory::new(20);
        let mut allowance = Allowance::new(&memory).unwrap();
        allowance.take(8).unwrap();
        allowance.take(8).unwrap();
        assert!(matches!(allowance.take(8), Err(ComputeError::MemoryLimit { .. })));
        assert_eq!((allowance.used(), memory.in_use()), (16, 16));
    }

    #[test]
    fn a_buffer_is_charged_before_it_is_allocated() {
        let memory = ComputeMemory::new(64);
        let (buffer, charge) = memory.filled(8, 1u64).unwrap();
        assert_eq!(buffer, [1; 8]);
        assert_eq!(charge.bytes(), 64);
        assert!(matches!(memory.filled(1, 0u8), Err(ComputeError::MemoryLimit { .. })));
        let unlimited = ComputeMemory::new(u64::MAX);
        assert_eq!(
            unlimited.filled(usize::MAX / 2, 0u64).unwrap_err(),
            ComputeError::OutOfMemory { bytes: u64::MAX }
        );
        assert_eq!(unlimited.in_use(), 0);
    }
}
