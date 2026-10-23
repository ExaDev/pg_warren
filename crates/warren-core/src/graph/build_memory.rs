//! The build memory budget (DESIGN.md §5.4, `warren.max_build_memory`):
//! every graph-sized buffer a build holds is charged to it before it is
//! allocated, and a charge past the limit is a
//! [`GraphError::BuildMemoryLimit`], never an out-of-memory abort.

use std::mem::size_of;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::GraphError;

/// A budget of build memory in bytes, recording its high-water mark.
/// Clones share the budget.
#[derive(Debug, Clone)]
pub struct BuildMemory(Arc<Budget>);

#[derive(Debug)]
struct Budget {
    limit: u64,
    in_use: AtomicU64,
    peak: AtomicU64,
}

impl BuildMemory {
    /// A budget of `limit` bytes, none in use.
    pub fn new(limit: u64) -> Self {
        Self(Arc::new(Budget { limit, in_use: AtomicU64::new(0), peak: AtomicU64::new(0) }))
    }

    /// A budget that never refuses, for builds outside a refresh.
    pub fn unlimited() -> Self {
        Self::new(u64::MAX)
    }

    /// The budget's limit in bytes.
    pub fn limit(&self) -> u64 {
        self.0.limit
    }

    /// The bytes currently charged.
    pub fn in_use(&self) -> u64 {
        self.0.in_use.load(Ordering::Relaxed)
    }

    /// The most bytes ever charged at once.
    pub fn peak(&self) -> u64 {
        self.0.peak.load(Ordering::Relaxed)
    }

    /// Whether `other` is this budget (a clone of it), not another.
    pub(crate) fn is(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// The bytes that may still be charged.
    pub fn remaining(&self) -> u64 {
        self.limit().saturating_sub(self.in_use())
    }

    /// Checks that `bytes` more would fit, without charging them, so that a
    /// build refuses before it starts work it could not finish.
    pub fn require(&self, bytes: u64) -> Result<(), GraphError> {
        within(self.in_use(), bytes, self.limit()).map(|_| ())
    }

    /// Charges `bytes` until the returned charge is dropped.
    pub fn charge(&self, bytes: u64) -> Result<BuildCharge, GraphError> {
        let mut charge = BuildCharge { memory: self.clone(), bytes: 0 };
        charge.grow(bytes)?;
        Ok(charge)
    }

    /// Adds `bytes` to the bytes in use, if that stays within the limit.
    fn reserve(&self, bytes: u64) -> Result<(), GraphError> {
        let budget = &self.0;
        let mut in_use = budget.in_use.load(Ordering::Relaxed);
        loop {
            let total = within(in_use, bytes, budget.limit)?;
            match budget.in_use.compare_exchange_weak(
                in_use,
                total,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    budget.peak.fetch_max(total, Ordering::Relaxed);
                    return Ok(());
                }
                Err(current) => in_use = current,
            }
        }
    }

    /// Adds `bytes` to the bytes in use even past the limit: for memory a
    /// caller already holds, such as an infallible clone.
    fn force(&self, bytes: u64) {
        let total = self.0.in_use.fetch_add(bytes, Ordering::Relaxed).saturating_add(bytes);
        self.0.peak.fetch_max(total, Ordering::Relaxed);
    }

    fn release(&self, bytes: u64) {
        self.0.in_use.fetch_sub(bytes, Ordering::Relaxed);
    }
}

/// `in_use + bytes`, if it is at most `limit`. A sum past `u64::MAX` is no
/// allocation at all, so it is out of memory whatever the limit.
fn within(in_use: u64, bytes: u64, limit: u64) -> Result<u64, GraphError> {
    let total =
        in_use.checked_add(bytes).ok_or(GraphError::OutOfMemory { elements: usize::MAX })?;
    match total <= limit {
        true => Ok(total),
        false => Err(GraphError::BuildMemoryLimit { requested: bytes, in_use, limit }),
    }
}

/// Bytes charged to a [`BuildMemory`], released when dropped.
#[derive(Debug)]
pub struct BuildCharge {
    memory: BuildMemory,
    bytes: u64,
}

impl BuildCharge {
    /// A charge of no bytes against `memory`.
    pub fn none(memory: &BuildMemory) -> Self {
        Self { memory: memory.clone(), bytes: 0 }
    }

    /// The bytes this charge holds.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The memory it is charged to.
    pub fn memory(&self) -> &BuildMemory {
        &self.memory
    }

    /// Charges `bytes` more, if that stays within the limit.
    pub fn grow(&mut self, bytes: u64) -> Result<(), GraphError> {
        self.memory.reserve(bytes)?;
        self.bytes += bytes;
        Ok(())
    }

    /// Releases `bytes` of this charge.
    pub fn shrink(&mut self, bytes: u64) {
        let bytes = bytes.min(self.bytes);
        self.memory.release(bytes);
        self.bytes -= bytes;
    }

    /// Takes over `other`'s bytes, so they are released with this charge.
    pub fn absorb(&mut self, mut other: Self) {
        let bytes = std::mem::take(&mut other.bytes);
        match self.memory.is(&other.memory) {
            true => self.bytes += bytes,
            false => other.memory.release(bytes),
        }
    }

    /// Sets the charge to exactly `bytes`, even past the limit: for memory
    /// already allocated, whose size the allocator decided.
    fn settle(&mut self, bytes: u64) {
        match bytes >= self.bytes {
            true => self.memory.force(bytes - self.bytes),
            false => self.memory.release(self.bytes - bytes),
        }
        self.bytes = bytes;
    }
}

impl Drop for BuildCharge {
    fn drop(&mut self) {
        self.memory.release(self.bytes);
    }
}

/// The bytes of `len` values of `T`.
pub(crate) fn bytes_of<T>(len: usize) -> u64 {
    (len as u64).saturating_mul(size_of::<T>() as u64)
}

/// A vector whose capacity is charged to a [`BuildMemory`]: capacity is
/// charged before it is reserved (with `try_reserve_exact`), and released
/// when the buffer shrinks or is dropped.
#[derive(Debug)]
pub struct BuildBuffer<T> {
    values: Vec<T>,
    charge: BuildCharge,
}

impl<T> BuildBuffer<T> {
    /// An empty buffer charged to `memory`.
    pub fn new(memory: &BuildMemory) -> Self {
        Self { values: Vec::new(), charge: BuildCharge::none(memory) }
    }

    /// An empty buffer with room for exactly `capacity` values.
    pub fn with_capacity(memory: &BuildMemory, capacity: usize) -> Result<Self, GraphError> {
        let mut buffer = Self::new(memory);
        buffer.reserve_exact(capacity)?;
        Ok(buffer)
    }

    /// `values`, already allocated, charged to `memory` even past its
    /// limit.
    pub fn from_vec(memory: &BuildMemory, values: Vec<T>) -> Self {
        let mut buffer = Self::new(memory);
        buffer.values = values;
        buffer.settle();
        buffer
    }

    /// The memory this buffer is charged to.
    pub fn memory(&self) -> &BuildMemory {
        &self.charge.memory
    }

    /// The heap bytes it holds (its capacity), all of them charged.
    pub fn bytes(&self) -> u64 {
        self.charge.bytes()
    }

    /// How many more values fit without reserving.
    pub fn capacity_left(&self) -> usize {
        self.values.capacity() - self.values.len()
    }

    /// Makes room for exactly `additional` more values.
    pub fn reserve_exact(&mut self, additional: usize) -> Result<(), GraphError> {
        let spare = self.values.capacity() - self.values.len();
        let Some(more) = additional.checked_sub(spare).filter(|&more| more > 0) else {
            return Ok(());
        };
        let bytes = bytes_of::<T>(more);
        self.charge.grow(bytes)?;
        if self.values.try_reserve_exact(additional).is_err() {
            self.charge.shrink(bytes);
            return Err(GraphError::OutOfMemory {
                elements: self.values.len().saturating_add(additional),
            });
        }
        self.settle();
        Ok(())
    }

    /// Appends a value, growing geometrically while the budget allows and
    /// by what still fits near its limit.
    pub fn push(&mut self, value: T) -> Result<(), GraphError> {
        if self.values.len() == self.values.capacity() {
            let room = growth(self.values.len(), self.memory(), size_of::<T>() as u64);
            self.reserve_exact(room)?;
        }
        self.values.push(value);
        Ok(())
    }

    /// Shortens the buffer to `len` values, keeping its capacity.
    pub fn truncate(&mut self, len: usize) {
        self.values.truncate(len);
    }

    /// Frees every value and the capacity that held them.
    pub fn release(&mut self) {
        self.values = Vec::new();
        self.settle();
    }

    /// Releases the capacity beyond the length.
    pub fn shrink_to_fit(&mut self) {
        self.values.shrink_to_fit();
        self.settle();
    }

    /// The values and their charge.
    pub fn into_parts(self) -> (Vec<T>, BuildCharge) {
        (self.values, self.charge)
    }

    /// Charges exactly the capacity held.
    fn settle(&mut self) {
        let held = bytes_of::<T>(self.values.capacity());
        self.charge.settle(held);
    }
}

impl<T: PartialEq> BuildBuffer<T> {
    /// Removes consecutive repeated values.
    pub fn dedup(&mut self) {
        self.values.dedup();
    }
}

impl<T> BuildBuffer<T> {
    /// Removes consecutive values that `same` says are equal, keeping the
    /// first of each run.
    pub fn dedup_by(&mut self, same: impl FnMut(&mut T, &mut T) -> bool) {
        self.values.dedup_by(same);
    }
}

impl<T: Clone> BuildBuffer<T> {
    /// A buffer of `len` copies of `value`.
    pub fn filled(memory: &BuildMemory, len: usize, value: T) -> Result<Self, GraphError> {
        let mut buffer = Self::with_capacity(memory, len)?;
        buffer.values.resize(len, value);
        Ok(buffer)
    }
}

/// How many values a full buffer of `len` values grows by, when each value
/// needs `per_value` bytes (of this buffer, and of others growing in
/// lockstep with it): as many again (at least 1024) while the budget
/// allows, else what still fits, and at least one, which the charge then
/// refuses if it does not fit.
pub(crate) fn growth(len: usize, memory: &BuildMemory, per_value: u64) -> usize {
    let fits = usize::try_from(memory.remaining() / per_value.max(1)).unwrap_or(usize::MAX);
    len.max(1024).min(fits).max(1)
}

impl<T> Deref for BuildBuffer<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.values
    }
}

impl<T> DerefMut for BuildBuffer<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.values
    }
}

impl<T: Clone> Clone for BuildBuffer<T> {
    /// A copy charged to the same budget, even past its limit.
    fn clone(&self) -> Self {
        Self::from_vec(self.memory(), self.values.clone())
    }
}

impl<T: PartialEq> PartialEq for BuildBuffer<T> {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
    }
}

impl<T: Eq> Eq for BuildBuffer<T> {}

impl<T> Extend<T> for BuildBuffer<T> {
    /// Appends within the capacity already reserved; callers reserve first.
    fn extend<I: IntoIterator<Item = T>>(&mut self, values: I) {
        self.values.extend(values);
        self.settle();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_are_released_when_dropped_and_the_peak_is_kept() {
        let memory = BuildMemory::new(100);
        let first = memory.charge(60).unwrap();
        assert_eq!(
            memory.charge(41).map(|charge| charge.bytes()),
            Err(GraphError::BuildMemoryLimit { requested: 41, in_use: 60, limit: 100 })
        );
        let second = memory.charge(40).unwrap();
        assert_eq!((memory.in_use(), memory.remaining()), (100, 0));
        drop((first, second));
        assert_eq!((memory.in_use(), memory.peak()), (0, 100));
    }

    #[test]
    fn require_checks_without_charging() {
        let memory = BuildMemory::new(10);
        let _held = memory.charge(4).unwrap();
        assert_eq!(memory.require(6), Ok(()));
        assert_eq!(
            memory.require(7),
            Err(GraphError::BuildMemoryLimit { requested: 7, in_use: 4, limit: 10 })
        );
        assert_eq!(memory.in_use(), 4);
    }

    #[test]
    fn a_buffer_is_charged_its_capacity() {
        let memory = BuildMemory::new(64);
        let mut buffer = BuildBuffer::<u64>::with_capacity(&memory, 4).unwrap();
        assert_eq!(memory.in_use(), 32);
        buffer.extend([1, 2]);
        buffer.shrink_to_fit();
        assert_eq!(memory.in_use(), 16);
        assert!(matches!(
            BuildBuffer::<u64>::with_capacity(&memory, 7),
            Err(GraphError::BuildMemoryLimit { .. })
        ));
        drop(buffer);
        assert_eq!(memory.in_use(), 0);
    }

    #[test]
    fn a_growing_buffer_takes_what_fits_near_the_limit() {
        let memory = BuildMemory::new(8 * 1500);
        let mut buffer = BuildBuffer::<u64>::new(&memory);
        for value in 0..1500 {
            buffer.push(value).unwrap();
        }
        assert_eq!(buffer.len(), 1500);
        assert_eq!(memory.in_use(), 8 * 1500);
        assert!(matches!(buffer.push(0), Err(GraphError::BuildMemoryLimit { .. })));
    }

    #[test]
    fn an_impossible_reservation_is_an_error_not_an_abort() {
        let mut buffer = BuildBuffer::<u64>::new(&BuildMemory::unlimited());
        assert!(matches!(
            buffer.reserve_exact(usize::MAX / 8),
            Err(GraphError::OutOfMemory { .. })
        ));
        assert_eq!(buffer.memory().in_use(), 0);
    }

    #[test]
    fn absorbed_charges_are_released_together() {
        let memory = BuildMemory::new(10);
        let mut all = memory.charge(3).unwrap();
        all.absorb(memory.charge(4).unwrap());
        assert_eq!((all.bytes(), memory.in_use()), (7, 7));
        drop(all);
        assert_eq!(memory.in_use(), 0);
    }
}
