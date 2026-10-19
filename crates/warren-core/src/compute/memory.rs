//! The compute memory budget: every vector, message buffer and work buffer
//! is charged to it, and a charge past its limit is an error rather than an
//! out-of-memory abort (DESIGN.md §6.9, `warren.max_compute_memory`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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
        let budget = &self.0;
        budget
            .in_use
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |in_use| {
                in_use.checked_add(bytes).filter(|&total| total <= budget.limit)
            })
            .map_err(|in_use| ComputeError::MemoryLimit {
                requested: bytes,
                in_use,
                limit: budget.limit,
            })?;
        Ok(MemoryCharge { memory: self.clone(), bytes })
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

impl Drop for MemoryCharge {
    fn drop(&mut self) {
        self.memory.0.in_use.fetch_sub(self.bytes, Ordering::Relaxed);
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
}
