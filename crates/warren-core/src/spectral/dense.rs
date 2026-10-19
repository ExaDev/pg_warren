//! Passes over dense per-node values on the compute threads: elementwise
//! updates, and reductions over fixed-size blocks whose partial results
//! are combined in block order, so a result is the same for every thread
//! count (DESIGN.md Appendix A.2).

use std::ops::Range;

use rayon::prelude::*;

use super::Resources;
use crate::compute::ComputeError;
use crate::interrupts::{Cancelled, Interrupts};

/// The elements one task handles; a task checks the cancellation flag
/// first. Small under test, so tests cross block boundaries.
pub(super) const BLOCK: usize = if cfg!(test) { 7 } else { 4096 };

/// `values[i] = f(i, values[i])` for every `i`, in parallel.
pub(super) fn update<I: Interrupts>(
    resources: &mut Resources<'_, I>,
    values: &mut [f64],
    f: impl Fn(usize, f64) -> f64 + Sync,
) -> Result<(), ComputeError> {
    resources.run(|cancellation| {
        values.par_chunks_mut(BLOCK).enumerate().try_for_each(|(block, chunk)| {
            if cancellation.is_cancelled() {
                return Err(Cancelled.into());
            }
            for (offset, value) in chunk.iter_mut().enumerate() {
                *value = f(block * BLOCK + offset, *value);
            }
            Ok(())
        })
    })
}

/// `Σ f(i)` over `0..len`: each block summed in index order, then the
/// block sums added in block order.
pub(super) fn sum<I: Interrupts>(
    resources: &mut Resources<'_, I>,
    len: usize,
    f: impl Fn(usize) -> f64 + Sync,
) -> Result<f64, ComputeError> {
    reduce(resources, len, |a, b| a + b, f)
}

/// `max f(i)` over `0..len` (0 when `len = 0`; `f` should be non-negative).
pub(super) fn max<I: Interrupts>(
    resources: &mut Resources<'_, I>,
    len: usize,
    f: impl Fn(usize) -> f64 + Sync,
) -> Result<f64, ComputeError> {
    reduce(resources, len, f64::max, f)
}

/// Folds `f(i)` over `0..len` with `combine`, from 0, block by block.
fn reduce<I: Interrupts>(
    resources: &mut Resources<'_, I>,
    len: usize,
    combine: impl Fn(f64, f64) -> f64 + Sync,
    f: impl Fn(usize) -> f64 + Sync,
) -> Result<f64, ComputeError> {
    let block = |rows: Range<usize>| rows.map(&f).fold(0.0, &combine);
    fold(resources, len, BLOCK, 0.0, block, &combine)
}

/// Splits `0..len` into blocks of `block` indexes, computes `f` of each
/// block in parallel, and combines the results with `combine` in block
/// order, starting from `identity`.
pub(super) fn fold<I: Interrupts, T: Clone + Send + Sync>(
    resources: &mut Resources<'_, I>,
    len: usize,
    block: usize,
    identity: T,
    f: impl Fn(Range<usize>) -> T + Sync,
    combine: impl Fn(T, T) -> T,
) -> Result<T, ComputeError> {
    let block = block.max(1);
    let (mut partials, _charge) = resources.memory.filled(len.div_ceil(block), identity.clone())?;
    resources.run(|cancellation| {
        partials.par_iter_mut().enumerate().try_for_each(|(index, partial)| {
            if cancellation.is_cancelled() {
                return Err(Cancelled.into());
            }
            *partial = f(index * block..len.min((index + 1) * block));
            Ok(())
        })
    })?;
    Ok(partials.into_iter().fold(identity, combine))
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;
    use crate::compute::{ComputeMemory, Workers};
    use crate::interrupts::Uninterrupted;

    fn with<R>(threads: usize, f: impl FnOnce(&mut Resources<'_, Uninterrupted>) -> R) -> R {
        let workers = Workers::new(threads).unwrap();
        let memory = ComputeMemory::new(1 << 20);
        f(&mut Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted })
    }

    #[test]
    fn update_maps_every_element_by_index() {
        let mut values = vec![1.0; 20];
        with(3, |r| update(r, &mut values, |i, v| v + i as f64)).unwrap();
        assert_eq!(values, (0..20).map(|i| 1.0 + i as f64).collect::<Vec<_>>());
    }

    #[test]
    fn sums_and_maxima_are_the_same_for_every_thread_count() {
        let f = |i: usize| ((i * 7919) % 101) as f64 / 3.0 + 1e-17 * i as f64;
        let sums: Vec<f64> = [1, 2, 4].map(|t| with(t, |r| sum(r, 100, f).unwrap())).to_vec();
        assert!(sums.iter().all(|&s| s.to_bits() == sums[0].to_bits()));
        let blocked: f64 =
            (0..100).step_by(BLOCK).map(|s| (s..(s + BLOCK).min(100)).map(f).sum::<f64>()).sum();
        assert_eq!(sums[0], blocked);
        assert_eq!(with(2, |r| max(r, 100, f)).unwrap(), (0..100).map(f).fold(0.0, f64::max));
        assert_eq!(with(2, |r| sum(r, 0, f)).unwrap(), 0.0);
    }

    #[test]
    fn partial_results_are_charged() {
        let workers = Workers::new(1).unwrap();
        let memory = ComputeMemory::new(8);
        let mut resources =
            Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted };
        assert!(sum(&mut resources, BLOCK, |_| 1.0).is_ok());
        assert!(matches!(
            sum(&mut resources, BLOCK + 1, |_| 1.0),
            Err(ComputeError::MemoryLimit { .. })
        ));
    }
}
