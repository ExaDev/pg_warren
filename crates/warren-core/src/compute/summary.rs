//! A summary of a vector's live elements: what `vector_stats` reports
//! (DESIGN.md §6.2): count, min, max, mean, sample standard deviation, l2
//! norm and the number of non-zero elements.

use super::reduce::{chunked, greatest, kept, least};
use super::{AnyVector, ComputeError, Workers};
use crate::graph::Tombstones;
use crate::interrupts::Interrupts;

/// The statistics of a vector's live elements. Undefined ones are `None`:
/// `min`, `max` and `mean` of no elements, and `sd` of fewer than two.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Summary {
    /// The number of elements.
    pub count: u64,
    /// The smallest element in SQL order.
    pub min: Option<f64>,
    /// The largest element in SQL order (`NaN` above every number).
    pub max: Option<f64>,
    /// The sum divided by the count.
    pub mean: Option<f64>,
    /// The sample standard deviation, as SQL `stddev`.
    pub sd: Option<f64>,
    /// The square root of the sum of squares.
    pub l2: f64,
    /// The number of non-zero elements (`NaN` is non-zero).
    pub nnz: u64,
}

/// One chunk's share of a summary: the count, sum, sum of squares and
/// extremes, and Welford's mean and sum of squared deviations.
#[derive(Debug, Clone, Copy, Default)]
struct Partial {
    count: u64,
    sum: f64,
    squares: f64,
    nnz: u64,
    min: Option<f64>,
    max: Option<f64>,
    mean: f64,
    deviations: f64,
}

impl Partial {
    fn push(mut self, value: f64) -> Self {
        self.count += 1;
        self.sum += value;
        self.squares += value * value;
        self.nnz += u64::from(value != 0.0);
        self.min = Some(self.min.map_or(value, |min| least(min, value)));
        self.max = Some(self.max.map_or(value, |max| greatest(max, value)));
        let delta = value - self.mean;
        self.mean += delta / self.count as f64;
        self.deviations += delta * (value - self.mean);
        self
    }

    /// `self` followed by `next` (Chan et al.'s pairwise combination).
    fn then(self, next: Self) -> Self {
        if next.count == 0 {
            return self;
        }
        if self.count == 0 {
            return next;
        }
        let count = self.count + next.count;
        let (a, b) = (self.count as f64, next.count as f64);
        let delta = next.mean - self.mean;
        let pick = |a: Option<f64>, b: Option<f64>, f: fn(f64, f64) -> f64| match (a, b) {
            (Some(a), Some(b)) => Some(f(a, b)),
            (a, b) => a.or(b),
        };
        Self {
            count,
            sum: self.sum + next.sum,
            squares: self.squares + next.squares,
            nnz: self.nnz + next.nnz,
            min: pick(self.min, next.min, least),
            max: pick(self.max, next.max, greatest),
            mean: (a * self.mean + b * next.mean) / count as f64,
            deviations: self.deviations + next.deviations + delta * delta * a * b / count as f64,
        }
    }
}

/// The summary of the elements of the rows of `x` that are not tombstoned.
/// Chunks are summarised in parallel and combined in chunk order, so the
/// result does not depend on the number of threads.
pub fn summarise<I: Interrupts>(
    workers: &Workers,
    interrupts: &mut I,
    x: &AnyVector,
    tombstones: &Tombstones,
) -> Result<Summary, ComputeError> {
    let width = x.width();
    let len = x.id_space() as usize * width;
    let skip = Some(tombstones);
    let partials = chunked(workers, interrupts, len, |range| {
        let values: Box<dyn Iterator<Item = f64>> = match x {
            AnyVector::F32(x) => Box::new(kept(x.values(), range, width, skip)),
            AnyVector::F64(x) => Box::new(kept(x.values(), range, width, skip)),
            AnyVector::I64(x) => Box::new(kept(x.values(), range, width, skip)),
            AnyVector::Bool(x) => Box::new(kept(x.values(), range, width, skip)),
        };
        values.fold(Partial::default(), Partial::push)
    })?;
    let total = partials.into_iter().fold(Partial::default(), Partial::then);
    let count = total.count as f64;
    Ok(Summary {
        count: total.count,
        min: total.min,
        max: total.max,
        mean: (total.count > 0).then(|| total.sum / count),
        sd: (total.count > 1).then(|| (total.deviations / (count - 1.0)).sqrt()),
        l2: total.squares.sqrt(),
        nnz: total.nnz,
    })
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;
    use crate::compute::{AnyVector, ComputeMemory, Vector, Workers};
    use crate::graph::Tombstones;
    use crate::interrupts::Uninterrupted;
    use std::sync::OnceLock;

    fn workers() -> &'static Workers {
        static WORKERS: OnceLock<Workers> = OnceLock::new();
        WORKERS.get_or_init(|| Workers::new(3).unwrap())
    }

    fn vector(width: usize, values: Vec<f64>) -> AnyVector {
        let id_space = (values.len() / width) as u64;
        Vector::from_values(&ComputeMemory::new(1 << 20), id_space, width, values).unwrap().into()
    }

    fn summary(x: &AnyVector, tombstoned: &[u64]) -> Summary {
        let mut tombstones = Tombstones::new(x.id_space()).unwrap();
        tombstoned.iter().for_each(|&id| tombstones.insert(id).unwrap());
        summarise(workers(), &mut Uninterrupted, x, &tombstones).unwrap()
    }

    #[test]
    fn a_summary_covers_the_elements_of_live_rows() {
        let x = vector(1, vec![2.0, 4.0, 100.0, 4.0, 4.0, 0.0, 5.0, 5.0, 7.0, 9.0]);
        let found = summary(&x, &[2]);
        assert_eq!(found.count, 9);
        assert_eq!((found.min, found.max), (Some(0.0), Some(9.0)));
        assert_eq!(found.mean, Some(40.0 / 9.0));
        let squares: f64 = [2.0f64, 4.0, 4.0, 4.0, 0.0, 5.0, 5.0, 7.0, 9.0]
            .iter()
            .map(|v| (v - 40.0 / 9.0).powi(2))
            .sum();
        let sd = found.sd.unwrap();
        assert!((sd - (squares / 8.0).sqrt()).abs() < 1e-12, "{sd}");
        assert_eq!(found.l2, 232f64.sqrt());
        assert_eq!(found.nnz, 8);
    }

    #[test]
    fn a_matrix_summary_covers_every_element_of_its_live_rows() {
        let x = vector(2, vec![1.0, -1.0, 8.0, 8.0, 3.0, 0.0]);
        let found = summary(&x, &[1]);
        assert_eq!((found.count, found.nnz), (4, 3));
        assert_eq!((found.min, found.max, found.mean), (Some(-1.0), Some(3.0), Some(0.75)));
    }

    #[test]
    fn too_few_elements_leave_statistics_undefined() {
        let empty = summary(&vector(1, vec![]), &[]);
        assert_eq!(empty, Summary { l2: 0.0, ..Summary::default() });
        let one = summary(&vector(1, vec![-3.0]), &[]);
        assert_eq!((one.mean, one.sd, one.min), (Some(-3.0), None, Some(-3.0)));
    }

    #[test]
    fn nan_makes_the_moments_nan_and_is_the_max() {
        let found = summary(&vector(1, vec![1.0, f64::NAN, 2.0]), &[]);
        assert!(found.mean.unwrap().is_nan() && found.sd.unwrap().is_nan());
        assert!(found.max.unwrap().is_nan());
        assert_eq!((found.min, found.nnz), (Some(1.0), 3));
    }
}
