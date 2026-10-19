//! The Lanczos basis: orthonormal vectors over the id space, stored as `f32`
//! or `f64` with every sum accumulated in `f64` (DESIGN.md §14 item 8).
//!
//! The basis is row-major (row `i` holds element `i` of every basis
//! vector), so each pass over it reads contiguous memory and splits into
//! blocks of rows. Each row's result is summed in basis order and block
//! partial sums are added in block order, so every pass gives the same
//! bits for any thread count.

use nalgebra::DMatrix;
use rayon::prelude::*;

use super::dense::BLOCK;
use super::Resources;
use crate::compute::{ComputeError, Element, MemoryCharge};
use crate::interrupts::{Cancelled, Interrupts};

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
    rows: usize,
    capacity: usize,
    len: usize,
    values: Vec<E>,
    _charge: MemoryCharge,
}

impl<E: Stored> Basis<E> {
    /// An empty basis with room for `capacity` vectors of `rows` elements.
    pub(super) fn new<I>(
        resources: &mut Resources<'_, I>,
        rows: usize,
        capacity: usize,
    ) -> Result<Self, ComputeError> {
        let len =
            rows.checked_mul(capacity).ok_or(ComputeError::OutOfMemory { bytes: u64::MAX })?;
        let (values, charge) = resources.memory.filled(len, E::default())?;
        Ok(Self { rows, capacity, len: 0, values, _charge: charge })
    }

    /// The number of basis vectors.
    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// Writes basis vector `column` to `out`.
    pub(super) fn column<I: Interrupts>(
        &self,
        resources: &mut Resources<'_, I>,
        column: usize,
        out: &mut [f64],
    ) -> Result<(), ComputeError> {
        let capacity = self.capacity;
        resources.run(|cancellation| {
            out.par_chunks_mut(BLOCK).zip(self.values.par_chunks(BLOCK * capacity)).try_for_each(
                |(out, rows)| {
                    if cancellation.is_cancelled() {
                        return Err(Cancelled.into());
                    }
                    for (out, row) in out.iter_mut().zip(rows.chunks(capacity)) {
                        *out = row[column].to_f64();
                    }
                    Ok(())
                },
            )
        })
    }

    /// Makes `w` orthogonal to every basis vector by classical Gram–Schmidt
    /// applied twice, returning the coefficients `Qᵀw` of both passes
    /// summed.
    pub(super) fn orthogonalize<I: Interrupts>(
        &self,
        resources: &mut Resources<'_, I>,
        w: &mut [f64],
    ) -> Result<Vec<f64>, ComputeError> {
        let mut coefficients = self.project(resources, w)?;
        self.subtract(resources, w, &coefficients)?;
        let correction = self.project(resources, w)?;
        self.subtract(resources, w, &correction)?;
        for (c, d) in coefficients.iter_mut().zip(correction) {
            *c += d;
        }
        Ok(coefficients)
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
        let (capacity, column) = (self.capacity, self.len);
        let values = &mut self.values;
        resources.run(|cancellation| {
            values.par_chunks_mut(BLOCK * capacity).zip(w.par_chunks(BLOCK)).try_for_each(
                |(rows, w)| {
                    if cancellation.is_cancelled() {
                        return Err(Cancelled.into());
                    }
                    for (row, &w) in rows.chunks_mut(capacity).zip(w) {
                        row[column] = E::from_f64(w / norm);
                    }
                    Ok(())
                },
            )
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
        let (capacity, len, kept) = (self.capacity, self.len, combinations.ncols());
        let values = &mut self.values;
        resources.run(|cancellation| {
            values.par_chunks_mut(BLOCK * capacity).try_for_each(|rows| {
                if cancellation.is_cancelled() {
                    return Err(Cancelled.into());
                }
                let mut rotated = vec![0.0; kept];
                for row in rows.chunks_mut(capacity) {
                    for (l, out) in rotated.iter_mut().enumerate() {
                        *out = (0..len).map(|j| combinations[(j, l)] * row[j].to_f64()).sum();
                    }
                    for (stored, &value) in row.iter_mut().zip(&rotated) {
                        *stored = E::from_f64(value);
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
        let (capacity, len) = (self.capacity, self.len);
        resources.run(|cancellation| {
            out.par_chunks_mut(BLOCK).zip(self.values.par_chunks(BLOCK * capacity)).try_for_each(
                |(out, rows)| {
                    if cancellation.is_cancelled() {
                        return Err(Cancelled.into());
                    }
                    for (out, row) in out.iter_mut().zip(rows.chunks(capacity)) {
                        *out = (0..len).map(|j| coefficients[j] * row[j].to_f64()).sum();
                    }
                    Ok(())
                },
            )
        })
    }

    /// `Qᵀw`.
    fn project<I: Interrupts>(
        &self,
        resources: &mut Resources<'_, I>,
        w: &[f64],
    ) -> Result<Vec<f64>, ComputeError> {
        let (capacity, len) = (self.capacity, self.len);
        let blocks = self.rows.div_ceil(BLOCK);
        let (mut partials, _charge) = resources.memory.filled(blocks * len.max(1), 0.0)?;
        resources.run(|cancellation| {
            partials
                .par_chunks_mut(len.max(1))
                .zip(self.values.par_chunks(BLOCK * capacity))
                .zip(w.par_chunks(BLOCK))
                .try_for_each(|((partial, rows), w)| {
                    if cancellation.is_cancelled() {
                        return Err(Cancelled.into());
                    }
                    for (row, &w) in rows.chunks(capacity).zip(w) {
                        for (sum, value) in partial.iter_mut().zip(&row[..len]) {
                            *sum += value.to_f64() * w;
                        }
                    }
                    Ok(())
                })
        })?;
        let mut coefficients = vec![0.0; len];
        for partial in partials.chunks(len.max(1)) {
            for (sum, &value) in coefficients.iter_mut().zip(partial) {
                *sum += value;
            }
        }
        Ok(coefficients)
    }

    /// `w −= Q·coefficients`.
    fn subtract<I: Interrupts>(
        &self,
        resources: &mut Resources<'_, I>,
        w: &mut [f64],
        coefficients: &[f64],
    ) -> Result<(), ComputeError> {
        let (capacity, len) = (self.capacity, self.len);
        resources.run(|cancellation| {
            w.par_chunks_mut(BLOCK).zip(self.values.par_chunks(BLOCK * capacity)).try_for_each(
                |(w, rows)| {
                    if cancellation.is_cancelled() {
                        return Err(Cancelled.into());
                    }
                    for (w, row) in w.iter_mut().zip(rows.chunks(capacity)) {
                        let sum: f64 = (0..len).map(|j| coefficients[j] * row[j].to_f64()).sum();
                        *w -= sum;
                    }
                    Ok(())
                },
            )
        })
    }
}
