//! The Lanczos basis: orthonormal vectors over the id space, stored as `f32`
//! or `f64` with every sum accumulated in `f64` (DESIGN.md §14 item 8).
//!
//! The basis is stored in blocks of [`BLOCK`] rows, and within a block
//! vector by vector, so a pass over the basis splits into independent row
//! blocks and reads each vector's segment contiguously. Each row's result
//! is summed in basis order and block partial sums are added in block
//! order, so every pass gives the same bits for any thread count.

use nalgebra::DMatrix;
use rayon::prelude::*;

use super::dense::BLOCK;
use super::Resources;
use crate::compute::{ComputeError, Element, MemoryCharge};
use crate::interrupts::{Cancellation, Cancelled, Interrupts};

/// The element type of the basis vectors.
pub(super) trait Stored: Element {
    /// `value` rounded to this type.
    fn from_f64(value: f64) -> Self;
}

impl Stored for f32 {
    fn from_f64(value: f64) -> Self {
        value as f32
    }
}

impl Stored for f64 {
    fn from_f64(value: f64) -> Self {
        value
    }
}

/// Up to `capacity` basis vectors of `rows` elements.
#[derive(Debug)]
pub(super) struct Basis<E> {
    capacity: usize,
    len: usize,
    /// Block `b` holds `BLOCK × capacity` elements: vector `j`'s rows of the
    /// block at `j·BLOCK..(j + 1)·BLOCK` (the last block padded with zeros).
    values: Vec<E>,
    _charge: MemoryCharge,
}

/// A vector made orthogonal to the basis: its coefficients `Qᵀw` and the
/// norm of what is left.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Orthogonalized {
    pub(super) coefficients: Vec<f64>,
    pub(super) norm: f64,
}

impl<E: Stored> Basis<E> {
    /// An empty basis with room for `capacity` vectors of `rows` elements.
    pub(super) fn new<I>(
        resources: &mut Resources<'_, I>,
        rows: usize,
        capacity: usize,
    ) -> Result<Self, ComputeError> {
        let len = rows
            .div_ceil(BLOCK)
            .checked_mul(BLOCK * capacity)
            .ok_or(ComputeError::OutOfMemory { bytes: u64::MAX })?;
        let (values, charge) = resources.memory.filled(len, E::default())?;
        Ok(Self { capacity, len: 0, values, _charge: charge })
    }

    /// The number of basis vectors.
    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// Runs `f(block's basis values, block's rows of w)` for every block in
    /// parallel, and returns the results in block order.
    fn blocks<I: Interrupts, R: Send>(
        &self,
        resources: &mut Resources<'_, I>,
        w: &mut [f64],
        f: impl Fn(&[E], &mut [f64]) -> R + Sync,
    ) -> Result<Vec<R>, ComputeError> {
        let values = &self.values;
        let width = BLOCK * self.capacity;
        resources.run(|cancellation| {
            values
                .par_chunks(width)
                .zip(w.par_chunks_mut(BLOCK))
                .map(|(block, w)| checked(cancellation).map(|()| f(block, w)))
                .collect()
        })
    }

    /// Writes basis vector `column` to `out`.
    pub(super) fn column<I: Interrupts>(
        &self,
        resources: &mut Resources<'_, I>,
        column: usize,
        out: &mut [f64],
    ) -> Result<(), ComputeError> {
        self.blocks(resources, out, |block, out| {
            for (out, value) in out.iter_mut().zip(&block[column * BLOCK..]) {
                *out = value.to_f64();
            }
        })?;
        Ok(())
    }

    /// Makes `w`, `M` times the newest basis vector `q_j`, orthogonal to the
    /// basis. With `previous = Some(β)`, `q_j` came from the plain
    /// three-term recurrence (`β` its coupling to `q_{j−1}`), so in exact
    /// arithmetic `w` lies in the span of `q_{j−1}`, `q_j` and a new
    /// direction: the recurrence removes the first two and one pass of
    /// classical Gram–Schmidt against the whole basis removes what rounding
    /// left. Otherwise (after a restart or a random vector) Gram–Schmidt
    /// runs twice.
    pub(super) fn orthogonalize<I: Interrupts>(
        &self,
        resources: &mut Resources<'_, I>,
        w: &mut [f64],
        previous: Option<f64>,
    ) -> Result<Orthogonalized, ComputeError> {
        let mut coefficients = vec![0.0; self.len];
        let pending = match (previous, self.len.checked_sub(1)) {
            (Some(beta), Some(j)) if j > 0 => {
                let alpha = self.recurrence(resources, w, j, beta)?;
                coefficients[j - 1] = beta;
                coefficients[j] = alpha;
                Some((j, alpha))
            }
            _ => {
                let first = self.project(resources, w, None)?;
                self.subtract(resources, w, &first)?;
                add(&mut coefficients, &first);
                None
            }
        };
        let correction = self.project(resources, w, pending)?;
        let norm = self.subtract(resources, w, &correction)?;
        add(&mut coefficients, &correction);
        Ok(Orthogonalized { coefficients, norm })
    }

    /// `w −= β·q_{j−1}`, then returns `α = q_jᵀw` (still to be removed).
    fn recurrence<I: Interrupts>(
        &self,
        resources: &mut Resources<'_, I>,
        w: &mut [f64],
        j: usize,
        beta: f64,
    ) -> Result<f64, ComputeError> {
        let partials = self.blocks(resources, w, |block, w| {
            for (w, p) in w.iter_mut().zip(&block[(j - 1) * BLOCK..]) {
                *w -= beta * p.to_f64();
            }
            dot(w, &block[j * BLOCK..])
        })?;
        Ok(partials.into_iter().fold(0.0, |a, b| a + b))
    }

    /// Appends `w / norm` as a basis vector; full bases ignore it.
    pub(super) fn push<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
        w: &[f64],
        norm: f64,
    ) -> Result<(), ComputeError> {
        if self.len == self.capacity {
            return Ok(());
        }
        let (width, column) = (BLOCK * self.capacity, self.len);
        let values = &mut self.values;
        resources.run(|cancellation| {
            values.par_chunks_mut(width).zip(w.par_chunks(BLOCK)).try_for_each(|(block, w)| {
                checked(cancellation)?;
                for (stored, &w) in block[column * BLOCK..].iter_mut().zip(w) {
                    *stored = E::from_f64(w / norm);
                }
                Ok(())
            })
        })?;
        self.len += 1;
        Ok(())
    }

    /// Replaces the basis `Q` by its first `combinations.ncols()` vectors
    /// `Q·combinations`, in place.
    pub(super) fn rotate<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
        combinations: &DMatrix<f64>,
    ) -> Result<(), ComputeError> {
        let (width, len, kept) = (BLOCK * self.capacity, self.len, combinations.ncols());
        let values = &mut self.values;
        resources.run(|cancellation| {
            values.par_chunks_mut(width).try_for_each(|block| {
                checked(cancellation)?;
                // A few rows at a time, so the scratch (`kept` sums per row)
                // stays in cache while every old vector's segment is added
                // to it, each row's sums in basis order.
                let mut sums = vec![0.0; kept * ROTATED_ROWS];
                for start in (0..BLOCK).step_by(ROTATED_ROWS) {
                    let rows = ROTATED_ROWS.min(BLOCK - start);
                    sums.fill(0.0);
                    for j in 0..len {
                        let old = &block[j * BLOCK + start..j * BLOCK + start + rows];
                        for (l, sum) in sums.chunks_exact_mut(ROTATED_ROWS).enumerate() {
                            let c = combinations[(j, l)];
                            for (sum, q) in sum.iter_mut().zip(old) {
                                *sum += c * q.to_f64();
                            }
                        }
                    }
                    for (l, sum) in sums.chunks_exact(ROTATED_ROWS).enumerate() {
                        let new = &mut block[l * BLOCK + start..l * BLOCK + start + rows];
                        for (stored, &sum) in new.iter_mut().zip(sum) {
                            *stored = E::from_f64(sum);
                        }
                    }
                }
                Ok(())
            })
        })?;
        self.len = kept;
        Ok(())
    }

    /// `out = Q·coefficients`.
    pub(super) fn combine<I: Interrupts>(
        &self,
        resources: &mut Resources<'_, I>,
        coefficients: &[f64],
        out: &mut [f64],
    ) -> Result<(), ComputeError> {
        self.blocks(resources, out, |block, out| {
            out.fill(0.0);
            for (j, &c) in coefficients.iter().enumerate() {
                for (out, value) in out.iter_mut().zip(&block[j * BLOCK..]) {
                    *out += c * value.to_f64();
                }
            }
        })?;
        Ok(())
    }

    /// `Qᵀw`, after `w −= c·q_j` for `pending = Some((j, c))`.
    fn project<I: Interrupts>(
        &self,
        resources: &mut Resources<'_, I>,
        w: &mut [f64],
        pending: Option<(usize, f64)>,
    ) -> Result<Vec<f64>, ComputeError> {
        let len = self.len;
        let partials = self.blocks(resources, w, |block, w| {
            if let Some((j, c)) = pending {
                for (w, q) in w.iter_mut().zip(&block[j * BLOCK..]) {
                    *w -= c * q.to_f64();
                }
            }
            (0..len).map(|j| dot(w, &block[j * BLOCK..])).collect::<Vec<f64>>()
        })?;
        let mut coefficients = vec![0.0; len];
        for partial in &partials {
            add(&mut coefficients, partial);
        }
        Ok(coefficients)
    }

    /// `w −= Q·coefficients`; returns `‖w‖₂` after.
    fn subtract<I: Interrupts>(
        &self,
        resources: &mut Resources<'_, I>,
        w: &mut [f64],
        coefficients: &[f64],
    ) -> Result<f64, ComputeError> {
        let partials = self.blocks(resources, w, |block, w| {
            for (j, &c) in coefficients.iter().enumerate() {
                for (w, q) in w.iter_mut().zip(&block[j * BLOCK..]) {
                    *w -= c * q.to_f64();
                }
            }
            dot(w, w)
        })?;
        Ok(partials.into_iter().fold(0.0, |a, b| a + b).sqrt())
    }
}

/// Fails if the job has been cancelled.
fn checked(cancellation: &Cancellation) -> Result<(), ComputeError> {
    match cancellation.is_cancelled() {
        true => Err(Cancelled.into()),
        false => Ok(()),
    }
}

/// The rows [`Basis::rotate`] combines at a time.
const ROTATED_ROWS: usize = if cfg!(test) { 3 } else { 256 };

/// The independent partial sums of [`dot`]: enough to hide the latency
/// of a floating-point add, so the sum is bound by memory, not by one
/// chain of adds.
const LANES: usize = 8;

/// `Σ a_i·b_i` over the common length, in `f64`: term `i` is added to
/// partial sum `i mod LANES`, and the partial sums are added in order, so
/// the result depends only on the inputs, never on the thread count.
fn dot<A: Element, B: Element>(a: &[A], b: &[B]) -> f64 {
    let len = a.len().min(b.len());
    let (a, b) = (&a[..len], &b[..len]);
    let mut lanes = [0.0f64; LANES];
    let (a_chunks, b_chunks) = (a.chunks_exact(LANES), b.chunks_exact(LANES));
    let (a_tail, b_tail) = (a_chunks.remainder(), b_chunks.remainder());
    for (a, b) in a_chunks.zip(b_chunks) {
        for ((lane, a), b) in lanes.iter_mut().zip(a).zip(b) {
            *lane += a.to_f64() * b.to_f64();
        }
    }
    for ((lane, a), b) in lanes.iter_mut().zip(a_tail).zip(b_tail) {
        *lane += a.to_f64() * b.to_f64();
    }
    lanes.iter().fold(0.0, |sum, lane| sum + lane)
}

/// `sum += part`, elementwise.
fn add(sum: &mut [f64], part: &[f64]) {
    for (sum, part) in sum.iter_mut().zip(part) {
        *sum += part;
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    #[cfg(not(miri))]
    use crate::compute::{ComputeMemory, Workers};
    #[cfg(not(miri))]
    use crate::interrupts::Uninterrupted;

    #[test]
    fn a_dot_product_sums_every_term_including_the_tail() {
        for len in [0, 1, 7, 8, 9, 31] {
            let w: Vec<f64> = (0..len).map(|i| f64::from(i as u32) + 1.0).collect();
            let q: Vec<f32> = (0..len).map(|i| if i % 2 == 0 { 2.0 } else { -1.0 }).collect();
            let exact: f64 = w.iter().zip(&q).map(|(w, q)| w * f64::from(*q)).sum();
            assert_eq!(dot(&w, &q), exact, "len {len}");
        }
    }

    /// Rotates a basis of `vectors` (rows × len) by `combinations` (len ×
    /// kept) and checks every new value against each row's combination
    /// summed in basis order, then rounded to `f32`: the bits, not a bound.
    #[cfg(not(miri))]
    fn rotation_is_exact(vectors: &[Vec<f64>], combinations: &DMatrix<f64>) {
        let workers = Workers::new(2).unwrap();
        let memory = ComputeMemory::new(1 << 22);
        let mut resources =
            Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted };
        let (rows, len, kept) = (vectors[0].len(), vectors.len(), combinations.ncols());
        let mut basis = Basis::<f32>::new(&mut resources, rows, len).unwrap();
        for v in vectors {
            basis.push(&mut resources, v, 1.0).unwrap();
        }
        let stored: Vec<Vec<f64>> = (0..len)
            .map(|j| {
                let mut out = vec![0.0; rows];
                basis.column(&mut resources, j, &mut out).unwrap();
                out
            })
            .collect();
        basis.rotate(&mut resources, combinations).unwrap();
        assert_eq!(basis.len(), kept);
        for l in 0..kept {
            let mut got = vec![0.0; rows];
            basis.column(&mut resources, l, &mut got).unwrap();
            let want: Vec<f64> = (0..rows)
                .map(|r| {
                    let sum = (0..len).fold(0.0, |s, j| s + combinations[(j, l)] * stored[j][r]);
                    f64::from(sum as f32)
                })
                .collect();
            assert_eq!(got, want, "vector {l}");
        }
    }

    // Not under Miri: it runs on the compute pool, whose work stealing goes
    // through crossbeam-epoch, which Miri's Stacked Borrows model rejects (a
    // known report against crossbeam-epoch 0.9, see compute::workers).
    #[cfg(not(miri))]
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn a_rotation_is_each_row_s_combination_summed_in_basis_order(
            (rows, len, kept) in (1usize..40, 1usize..7).prop_flat_map(|(r, l)| (Just(r), Just(l), 1..=l)),
            seed in any::<u64>(),
        ) {
            let value = |i: u64| crate::spectral::random::uniform(seed, 0, i);
            let vectors: Vec<Vec<f64>> =
                (0..len).map(|j| (0..rows).map(|r| value((j * rows + r) as u64)).collect()).collect();
            let combinations =
                DMatrix::from_fn(len, kept, |j, l| value(10_000 + (j * kept + l) as u64));
            rotation_is_exact(&vectors, &combinations);
        }
    }

    #[test]
    fn a_dot_product_reads_only_the_common_length() {
        assert_eq!(dot(&[1.0, 2.0], &[3.0f64, 4.0, 100.0]), 11.0);
    }

    proptest! {
        #[test]
        fn a_dot_product_is_the_sum_up_to_rounding(
            terms in prop::collection::vec((-1e3f64..1e3, -1e3f64..1e3), 0..200),
        ) {
            let (w, q): (Vec<f64>, Vec<f64>) = terms.into_iter().unzip();
            let sum: f64 = w.iter().zip(&q).map(|(a, b)| a * b).sum();
            let bound: f64 = w.iter().zip(&q).map(|(a, b)| (a * b).abs()).sum::<f64>() * 1e-12;
            prop_assert!((dot(&w, &q) - sum).abs() <= bound);
        }
    }
}
