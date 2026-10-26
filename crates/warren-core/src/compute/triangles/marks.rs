//! The scratch a `triangles` search marks its node's row in: one array
//! per thread of the compute pool, each a count per node, charged to the
//! compute budget.

use std::mem::size_of;
use std::sync::{Mutex, PoisonError};

use super::super::{ComputeError, ComputeMemory, MemoryCharge};

/// One array per pool thread. In a search from `u`, entry `w` is 0 unless
/// `w` is in `u`'s row, and then 1 plus the triangles found through `w`
/// so far: at most `u`'s row length, which is below `√(2M)` (a row holds
/// neighbours of no lower degree), so a `u32` cannot overflow.
pub(super) struct Marks {
    threads: Vec<Mutex<Vec<u32>>>,
    _charges: Vec<MemoryCharge>,
}

impl Marks {
    /// The bytes [`Self::new`] charges.
    pub(super) fn bytes(id_space: u64, threads: usize) -> u64 {
        id_space.saturating_mul(threads as u64).saturating_mul(size_of::<u32>() as u64)
    }

    /// An array of `nodes` zeros for each of `threads` threads.
    pub(super) fn new(
        memory: &ComputeMemory,
        nodes: usize,
        threads: usize,
    ) -> Result<Self, ComputeError> {
        let (mut arrays, mut charges) = (Vec::new(), Vec::new());
        for _ in 0..threads {
            let (array, charge) = memory.filled(nodes, 0u32)?;
            arrays.push(Mutex::new(array));
            charges.push(charge);
        }
        Ok(Self { threads: arrays, _charges: charges })
    }

    /// Runs `search` with the calling pool thread's array. Each thread
    /// locks only its own, and `search` must not wait on the pool (no
    /// nested parallel work), so the lock is never contended; off the pool
    /// the first array is used, and the lock keeps that sound too. An array
    /// left marked by a failed search (or a poisoned lock) is never read
    /// again: the count is abandoned.
    pub(super) fn with<R>(&self, search: impl FnOnce(&mut [u32]) -> R) -> R {
        let thread = rayon::current_thread_index().unwrap_or(0) % self.threads.len();
        let mut array = self.threads[thread].lock().unwrap_or_else(PoisonError::into_inner);
        search(&mut array)
    }
}
