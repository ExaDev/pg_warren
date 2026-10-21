//! Reductions of a vector to one number, and the norm of the difference of
//! two vectors (DESIGN.md §6.3, §7: `vector_reduce`, `vector_norm_diff`).
//!
//! Elements are read as `f64` (`bool` as 0 or 1). Chunks of a fixed size
//! are reduced in parallel and their partial results combined in chunk
//! order, so a result does not depend on the number of threads.

use std::fmt::{self, Display};
use std::ops::Range;
use std::str::FromStr;

use rayon::prelude::*;

use super::{AnyVector, ComputeError, Element, Workers};
use crate::graph::Tombstones;
use crate::interrupts::{Cancelled, Interrupts};

/// The elements each parallel task reduces; small under test, so tests
/// combine several partial results.
const CHUNK: usize = if cfg!(test) { 4 } else { 1 << 16 };

/// A reduction of every element of a vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reduction {
    /// The sum.
    Sum,
    /// The largest element in SQL order (`NaN` above every number).
    Max,
    /// The smallest element in SQL order.
    Min,
    /// The sum of absolute values.
    L1,
    /// The square root of the sum of squares.
    L2,
    /// The number of non-zero elements (`NaN` is non-zero).
    Nnz,
}

impl Reduction {
    const ALL: [Self; 6] = [Self::Sum, Self::Max, Self::Min, Self::L1, Self::L2, Self::Nnz];

    /// The name `vector_reduce` uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::Sum => "sum",
            Self::Max => "max",
            Self::Min => "min",
            Self::L1 => "l1",
            Self::L2 => "l2",
            Self::Nnz => "nnz",
        }
    }
}

impl Display for Reduction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Reduction {
    type Err = ComputeError;

    fn from_str(name: &str) -> Result<Self, ComputeError> {
        Self::ALL
            .into_iter()
            .find(|reduction| reduction.name() == name)
            .ok_or_else(|| ComputeError::UnknownReduction(name.to_owned()))
    }
}

/// A norm of the difference of two vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Norm {
    /// The sum of absolute differences.
    L1,
    /// The Euclidean norm.
    L2,
    /// The largest absolute difference.
    Linf,
}

impl Norm {
    const ALL: [Self; 3] = [Self::L1, Self::L2, Self::Linf];

    /// The name `vector_norm_diff` uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::L1 => "l1",
            Self::L2 => "l2",
            Self::Linf => "linf",
        }
    }
}

impl Display for Norm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Norm {
    type Err = ComputeError;

    fn from_str(name: &str) -> Result<Self, ComputeError> {
        Self::ALL
            .into_iter()
            .find(|norm| norm.name() == name)
            .ok_or_else(|| ComputeError::UnknownNorm(name.to_owned()))
    }
}

/// `reduction` over every element of `x`; `None` (SQL NULL) for the
/// `max` or `min` of no elements.
pub fn reduce<I: Interrupts>(
    workers: &Workers,
    interrupts: &mut I,
    x: &AnyVector,
    reduction: Reduction,
) -> Result<Option<f64>, ComputeError> {
    reduce_rows(workers, interrupts, x, None, reduction)
}

/// `reduction` over the elements of the rows of `x` that are not
/// tombstoned: the live nodes of a version.
pub fn reduce_live<I: Interrupts>(
    workers: &Workers,
    interrupts: &mut I,
    x: &AnyVector,
    tombstones: &Tombstones,
    reduction: Reduction,
) -> Result<Option<f64>, ComputeError> {
    reduce_rows(workers, interrupts, x, Some(tombstones), reduction)
}

/// `reduction` over the elements of `x`, skipping the rows `skip` marks.
fn reduce_rows<I: Interrupts>(
    workers: &Workers,
    interrupts: &mut I,
    x: &AnyVector,
    skip: Option<&Tombstones>,
    reduction: Reduction,
) -> Result<Option<f64>, ComputeError> {
    let width = x.width();
    let len = x.id_space() as usize * width;
    let partials = chunked(workers, interrupts, len, |range| match x {
        AnyVector::F32(x) => partial(kept(x.values(), range, width, skip), reduction),
        AnyVector::F64(x) => partial(kept(x.values(), range, width, skip), reduction),
        AnyVector::I64(x) => partial(kept(x.values(), range, width, skip), reduction),
        AnyVector::Bool(x) => partial(kept(x.values(), range, width, skip), reduction),
    })?;
    Ok(combine(partials, reduction))
}

/// The elements of `values` in `range` as `f64`, skipping those in rows
/// (of `width` elements) that `skip` marks.
pub(super) fn kept<'a, E: Element>(
    values: &'a [E],
    range: Range<usize>,
    width: usize,
    skip: Option<&'a Tombstones>,
) -> impl Iterator<Item = f64> + 'a {
    let start = range.start;
    values[range].iter().enumerate().filter_map(move |(offset, value)| {
        let row = || ((start + offset) / width) as u64;
        match skip.is_some_and(|skip| skip.contains(row())) {
            true => None,
            false => Some(value.to_f64()),
        }
    })
}

/// `norm(a − b)`, aligning `a` and `b` by dense id: a row past the end of
/// the shorter vector (one bound to an earlier graph version, before the
/// id space grew) counts as zero.
pub fn norm_diff<I: Interrupts>(
    workers: &Workers,
    interrupts: &mut I,
    a: &AnyVector,
    b: &AnyVector,
    norm: Norm,
) -> Result<f64, ComputeError> {
    if a.width() != b.width() {
        return Err(ComputeError::WidthMismatch { expected: a.width(), found: b.width() });
    }
    let len = a.id_space().max(b.id_space()) as usize * a.width();
    let reduction = match norm {
        Norm::L1 => Reduction::L1,
        Norm::L2 => Reduction::L2,
        Norm::Linf => Reduction::Max,
    };
    let partials = chunked(workers, interrupts, len, |range| {
        let (a, b) = (padded(a, range.clone()), padded(b, range));
        let differences = a.iter().zip(&b).map(|(a, b)| a - b);
        match norm {
            Norm::Linf => partial(differences.map(f64::abs), reduction),
            Norm::L1 | Norm::L2 => partial(differences, reduction),
        }
    })?;
    Ok(combine(partials, reduction).unwrap_or(0.0))
}

/// `partial` of each chunk of `0..len`, in parallel, in chunk order.
pub(super) fn chunked<I: Interrupts, T: Send>(
    workers: &Workers,
    interrupts: &mut I,
    len: usize,
    partial: impl Fn(Range<usize>) -> T + Sync,
) -> Result<Vec<T>, ComputeError> {
    workers.run(interrupts, |cancellation| {
        (0..len.div_ceil(CHUNK))
            .into_par_iter()
            .map(|chunk| match cancellation.is_cancelled() {
                true => Err(Cancelled.into()),
                false => Ok(partial(chunk * CHUNK..len.min((chunk + 1) * CHUNK))),
            })
            .collect()
    })
}

/// The elements of `x` in `range` as `f64`, zero past its end.
fn padded(x: &AnyVector, range: Range<usize>) -> Vec<f64> {
    fn read<E: Element>(values: &[E], range: Range<usize>) -> Vec<f64> {
        range.map(|i| values.get(i).map_or(0.0, |value| value.to_f64())).collect()
    }
    match x {
        AnyVector::F32(x) => read(x.values(), range),
        AnyVector::F64(x) => read(x.values(), range),
        AnyVector::I64(x) => read(x.values(), range),
        AnyVector::Bool(x) => read(x.values(), range),
    }
}

/// The larger in SQL order; the first on a tie.
pub(super) fn greatest(a: f64, b: f64) -> f64 {
    if b.sql_cmp(a).is_gt() {
        b
    } else {
        a
    }
}

/// The smaller in SQL order; the first on a tie.
pub(super) fn least(a: f64, b: f64) -> f64 {
    if b.sql_cmp(a).is_lt() {
        b
    } else {
        a
    }
}

/// `reduction` over one chunk: sums of the terms (squares for `l2`), or
/// the extreme element.
fn partial(values: impl Iterator<Item = f64>, reduction: Reduction) -> Option<f64> {
    match reduction {
        Reduction::Sum => Some(total(values)),
        Reduction::L1 => Some(total(values.map(f64::abs))),
        Reduction::L2 => Some(total(values.map(|value| value * value))),
        Reduction::Nnz => Some(values.filter(|value| value.is_nonzero()).count() as f64),
        Reduction::Max => values.reduce(greatest),
        Reduction::Min => values.reduce(least),
    }
}

/// The sum of `terms` in order, starting from +0.
fn total(terms: impl Iterator<Item = f64>) -> f64 {
    terms.fold(0.0, |sum, term| sum + term)
}

/// The chunks' partial results combined in chunk order.
fn combine(partials: Vec<Option<f64>>, reduction: Reduction) -> Option<f64> {
    let partials = partials.into_iter().flatten();
    match reduction {
        Reduction::Sum | Reduction::L1 | Reduction::Nnz => Some(total(partials)),
        Reduction::L2 => Some(total(partials).sqrt()),
        Reduction::Max => partials.reduce(greatest),
        Reduction::Min => partials.reduce(least),
    }
}

// Not under Miri: the compute pool's work stealing runs through
// crossbeam-epoch, which Miri's Stacked Borrows model rejects (a known
// report against crossbeam-epoch 0.9, not this crate, which has no unsafe).
#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;
    use crate::compute::{ComputeMemory, Element, Vector};
    use crate::interrupts::Uninterrupted;
    use proptest::prelude::*;
    use std::sync::OnceLock;

    fn workers() -> &'static Workers {
        static WORKERS: OnceLock<Workers> = OnceLock::new();
        WORKERS.get_or_init(|| Workers::new(3).unwrap())
    }

    fn vector<E: Element>(width: usize, values: Vec<E>) -> AnyVector
    where
        AnyVector: From<Vector<E>>,
    {
        let id_space = (values.len() / width) as u64;
        let memory = ComputeMemory::new(1 << 20);
        Vector::from_values(&memory, id_space, width, values).unwrap().into()
    }

    fn reduced(x: &AnyVector, reduction: &str) -> Option<f64> {
        reduce(workers(), &mut Uninterrupted, x, reduction.parse().unwrap()).unwrap()
    }

    fn diff(a: &AnyVector, b: &AnyVector, norm: &str) -> f64 {
        norm_diff(workers(), &mut Uninterrupted, a, b, norm.parse().unwrap()).unwrap()
    }

    #[test]
    fn names_parse() {
        for reduction in Reduction::ALL {
            assert_eq!(reduction.to_string().parse(), Ok(reduction));
        }
        for norm in Norm::ALL {
            assert_eq!(norm.to_string().parse(), Ok(norm));
        }
        assert_eq!("avg".parse::<Reduction>(), Err(ComputeError::UnknownReduction("avg".into())));
        assert_eq!("l3".parse::<Norm>(), Err(ComputeError::UnknownNorm("l3".into())));
    }

    #[test]
    fn reductions_of_a_matrix_cover_every_element() {
        let x = vector(2, vec![3.0, -4.0, 0.0, 1.0]);
        let found: Vec<_> =
            ["sum", "max", "min", "l1", "l2", "nnz"].iter().map(|r| reduced(&x, r)).collect();
        let l2 = 26f64.sqrt();
        assert_eq!(found, [Some(0.0), Some(3.0), Some(-4.0), Some(8.0), Some(l2), Some(3.0)]);
    }

    #[test]
    fn integers_and_booleans_reduce_as_numbers() {
        assert_eq!(reduced(&vector(1, vec![2i64, -5, 7]), "sum"), Some(4.0));
        assert_eq!(reduced(&vector(1, vec![true, false, true]), "sum"), Some(2.0));
        assert_eq!(reduced(&vector(1, vec![true, false, true]), "nnz"), Some(2.0));
        assert_eq!(reduced(&vector(1, vec![1.5f32, -0.5]), "min"), Some(-0.5));
    }

    #[test]
    fn nan_sorts_above_every_number() {
        let x = vector(1, vec![1.0, f64::NAN, -2.0]);
        assert!(reduced(&x, "max").unwrap().is_nan());
        assert_eq!(reduced(&x, "min"), Some(-2.0));
        assert_eq!(reduced(&x, "nnz"), Some(3.0));
        assert!(reduced(&x, "sum").unwrap().is_nan());
    }

    #[test]
    fn an_empty_vector_has_no_max_or_min() {
        let x = vector::<f64>(1, vec![]);
        assert_eq!(reduced(&x, "max"), None);
        assert_eq!(reduced(&x, "min"), None);
        assert_eq!(reduced(&x, "sum"), Some(0.0));
        assert_eq!(reduced(&x, "l2"), Some(0.0));
        assert_eq!(reduced(&x, "nnz"), Some(0.0));
    }

    fn tombstoned(id_space: u64, ids: &[u64]) -> Tombstones {
        let mut tombstones = Tombstones::new(id_space).unwrap();
        ids.iter().for_each(|&id| tombstones.insert(id).unwrap());
        tombstones
    }

    #[test]
    fn a_live_reduction_skips_every_element_of_tombstoned_rows() {
        let x = vector(2, vec![3.0, -4.0, 0.0, -9.0, 1.0, 5.0]);
        let tombstones = tombstoned(3, &[1]);
        let found: Vec<_> = Reduction::ALL
            .iter()
            .map(|&r| reduce_live(workers(), &mut Uninterrupted, &x, &tombstones, r).unwrap())
            .collect();
        let l2 = 51f64.sqrt();
        assert_eq!(found, [Some(5.0), Some(5.0), Some(-4.0), Some(13.0), Some(l2), Some(4.0)]);
        let all_gone = tombstoned(3, &[0, 1, 2]);
        assert_eq!(
            reduce_live(workers(), &mut Uninterrupted, &x, &all_gone, Reduction::Min).unwrap(),
            None
        );
    }

    #[test]
    fn norm_diff_treats_rows_missing_from_the_shorter_vector_as_zero() {
        let old = vector(1, vec![1.0, 2.0]);
        let new = vector(1, vec![1.0, 5.0, -4.0]);
        assert_eq!(diff(&new, &old, "l1"), 7.0);
        assert_eq!(diff(&old, &new, "l2"), 5.0);
        assert_eq!(diff(&old, &new, "linf"), 4.0);
        let ints = vector(1, vec![1i64, 2]);
        assert_eq!(diff(&ints, &old, "l1"), 0.0);
        assert_eq!(diff(&vector::<f64>(1, vec![]), &vector::<f64>(1, vec![]), "linf"), 0.0);
    }

    #[test]
    fn norm_diff_needs_equal_widths() {
        let a = vector(2, vec![1.0; 4]);
        let b = vector(1, vec![1.0; 2]);
        assert_eq!(
            norm_diff(workers(), &mut Uninterrupted, &a, &b, Norm::L1),
            Err(ComputeError::WidthMismatch { expected: 2, found: 1 })
        );
    }

    #[test]
    fn an_interrupt_cancels_a_reduction() {
        struct Stop;
        impl Interrupts for Stop {
            fn check(&mut self) -> Result<(), crate::interrupts::Cancelled> {
                Err(crate::interrupts::Cancelled)
            }
        }
        let x = vector(1, vec![1.0; 4]);
        assert_eq!(reduce(workers(), &mut Stop, &x, Reduction::Sum), Err(ComputeError::Cancelled));
        assert_eq!(norm_diff(workers(), &mut Stop, &x, &x, Norm::L2), Err(ComputeError::Cancelled));
    }

    fn values() -> impl Strategy<Value = Vec<f64>> {
        let value = prop_oneof![8 => (-8i8..9).prop_map(f64::from), 1 => Just(f64::NAN)];
        prop::collection::vec(value, 0..40)
    }

    fn naive(values: &[f64], reduction: Reduction) -> Option<f64> {
        let greatest = |a: f64, b: f64| if b.sql_cmp(a).is_gt() { b } else { a };
        let least = |a: f64, b: f64| if b.sql_cmp(a).is_lt() { b } else { a };
        let iter = values.iter().copied();
        match reduction {
            Reduction::Sum => Some(iter.sum()),
            Reduction::Max => iter.reduce(greatest),
            Reduction::Min => iter.reduce(least),
            Reduction::L1 => Some(iter.map(f64::abs).sum()),
            Reduction::L2 => Some(iter.map(|v| v * v).sum::<f64>().sqrt()),
            Reduction::Nnz => Some(iter.filter(|v| v.is_nonzero()).count() as f64),
        }
    }

    fn same(a: Option<f64>, b: Option<f64>) -> bool {
        match (a, b) {
            (Some(a), Some(b)) => a.sql_cmp(b).is_eq(),
            (a, b) => a.is_none() && b.is_none(),
        }
    }

    proptest! {
        #[test]
        fn reductions_match_a_naive_fold(values in values()) {
            let x = vector(1, values.clone());
            for reduction in Reduction::ALL {
                let found = reduce(workers(), &mut Uninterrupted, &x, reduction).unwrap();
                prop_assert!(same(found, naive(&values, reduction)), "{reduction}: {found:?}");
            }
        }

        #[test]
        fn norm_diff_matches_a_naive_fold(a in values(), b in values()) {
            let len = a.len().max(b.len());
            let at = |v: &[f64], i: usize| v.get(i).copied().unwrap_or(0.0);
            let d: Vec<f64> = (0..len).map(|i| at(&a, i) - at(&b, i)).collect();
            let (va, vb) = (vector(1, a.clone()), vector(1, b.clone()));
            for (norm, reduction) in [(Norm::L1, Reduction::L1), (Norm::L2, Reduction::L2)] {
                let found = norm_diff(workers(), &mut Uninterrupted, &va, &vb, norm).unwrap();
                prop_assert!(same(Some(found), naive(&d, reduction)));
            }
            let linf = norm_diff(workers(), &mut Uninterrupted, &va, &vb, Norm::Linf).unwrap();
            let abs: Vec<f64> = d.iter().map(|v| v.abs()).collect();
            prop_assert!(same(Some(linf), naive(&abs, Reduction::Max).or(Some(0.0))));
        }
    }
}
