//! The compute memory budget: every vector, message buffer and work buffer
//! is charged to it, and a charge past its limit is an error rather than an
//! out-of-memory abort (DESIGN.md §6.9, `warren.max_compute_memory`).

use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::message::{growth, reserve};
use super::ComputeError;

/// A budget of compute memory in bytes. Clones share the budget.
#[derive(Debug, Clone)]
pub struct ComputeMemory(Arc<Budget>);

#[derive(Debug)]
struct Budget {
    limit: u64,
    in_use: AtomicU64,
}

impl ComputeMemory {
    /// A budget of `limit` bytes, none in use.
    pub fn new(limit: u64) -> Self {
        Self(Arc::new(Budget { limit, in_use: AtomicU64::new(0) }))
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

    /// Adds `bytes` to the bytes in use, if that stays within the limit.
    fn reserve(&self, bytes: u64) -> Result<(), ComputeError> {
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
