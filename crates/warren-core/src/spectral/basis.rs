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
            let (previous, newest) = (&block[(j - 1) * BLOCK..], &block[j * BLOCK..]);
            let mut alpha = 0.0;
            for ((w, p), q) in w.iter_mut().zip(previous).zip(newest) {
                *w -= beta * p.to_f64();
                alpha += q.to_f64() * *w;
            }
            alpha
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
                let mut rotated = vec![0.0; BLOCK * kept];
                for (l, out) in rotated.chunks_mut(BLOCK).enumerate() {
                    for j in 0..len {
                        let c = combinations[(j, l)];
                        for (out, value) in out.iter_mut().zip(&block[j * BLOCK..]) {
                            *out += c * value.to_f64();
                        }
                    }
                }
                for (stored, &value) in block.iter_mut().zip(&rotated) {
                    *stored = E::from_f64(value);
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
            let dot = |j: usize| {
                w.iter().zip(&block[j * BLOCK..]).fold(0.0, |sum, (w, q)| sum + q.to_f64() * w)
            };
            (0..len).map(dot).collect::<Vec<f64>>()
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
            w.iter().map(|w| w * w).sum::<f64>()
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

/// `sum += part`, elementwise.
fn add(sum: &mut [f64], part: &[f64]) {
    for (sum, part) in sum.iter_mut().zip(part) {
        *sum += part;
    }
}
