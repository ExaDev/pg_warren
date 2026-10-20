//! Lanczos eigenpairs of a graph operator (DESIGN.md §6.5, §14 item 8).
//!
//! Lanczos with full reorthogonalisation and thick restarts: the basis
//! holds `max(2k + 1, 20)` vectors (at most the dimension); when it is full,
//! the Rayleigh–Ritz pairs of `H = QᵀMQ` are computed, and unless the `k`
//! wanted ones have converged the basis is compressed to the `k` wanted
//! Ritz vectors plus one per converged pair (between `(size − k)/4` and
//! `(size − k)/2` more) and extended again from the last residual. Each step removes the
//! three-term recurrence's two vectors and then makes one classical
//! Gram–Schmidt pass against the whole basis; the first step after a
//! restart or a random vector, whose product couples to every kept vector,
//! makes two. `H` is kept dense (it is tridiagonal plus the restart's
//! arrowhead), and its entries are the reorthogonalisation coefficients,
//! so it is the projection onto the basis as stored.
//!
//! The smallest eigenvalues are taken from the low end of the same Krylov
//! space. No spectral shift is applied: a Krylov space of `σI − M` is that
//! of `M`, so a shift gives the same Ritz values, and shift-and-invert
//! would need linear solves.
//!
//! A pair has converged when its residual estimate `|β·s_last|` is at most
//! `tol · max|θ|`. The residual reported is computed explicitly,
//! `‖Mx − θx‖₂`, for the eigenvector as stored. With `f32` vectors it does
//! not fall much below `1e-7 · ‖M‖`, the rounding of the stored vectors.
//! From one start vector, Lanczos finds one direction of each eigenspace,
//! so the copies of a repeated eigenvalue are found only when rounding or a
//! restart from a fresh vector (after an exact invariant subspace) brings
//! them in; graphs with several connected components have a repeated
//! Laplacian eigenvalue 0.
//!
//! Every vector is a seeded function of the graph, and every reduction is
//! order-deterministic, so the result is the same for any thread count.

use std::mem::size_of;
use std::str::FromStr;

use nalgebra::DMatrix;

use super::basis::{Basis, Orthogonalized, Stored};
use super::operator::LinearOperator;
use super::ritz::Ritz;
use super::working_set::WorkingSet;
use super::{dense, GraphMatrix, Resources, SpectralError};
use crate::compute::{AnyVector, ComputeError, ComputeMemory, MemoryCharge, Vector};
use crate::graph::GraphCsr;
use crate::interrupts::Interrupts;

/// The relative rounding of Ritz values from `f32` and `f64` vectors.
const F32_TIE: f64 = 1e-6;
const F64_TIE: f64 = 1e-12;

/// The default `max_iter`, per basis vector: enough for the crowded
/// low end of a random graph's normalised Laplacian at 10⁷ nodes (about
/// 14,500 steps for k = 20, DESIGN.md §12).
const STEPS_PER_BASIS_VECTOR: usize = 1000;

/// A new direction shorter than this, relative to the vector it came from,
/// means the basis spans an invariant subspace.
const BREAKDOWN: f64 = 1e-10;

/// Which end of the spectrum to compute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Which {
    /// The eigenvalues of largest magnitude, in descending magnitude.
    Largest,
    /// The smallest (most negative) eigenvalues, ascending.
    Smallest,
}

impl Which {
    /// The name the SQL `which` argument uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::Largest => "largest",
            Self::Smallest => "smallest",
        }
    }
}

impl FromStr for Which {
    type Err = SpectralError;

    fn from_str(name: &str) -> Result<Self, SpectralError> {
        [Self::Largest, Self::Smallest]
            .into_iter()
            .find(|which| which.name() == name)
            .ok_or_else(|| SpectralError::UnknownWhich(name.to_owned()))
    }
}

/// The element type of the Lanczos vectors (§14 item 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Precision {
    /// `f32` vectors, every sum accumulated in `f64`: half the memory.
    F32,
    /// `f64` vectors.
    F64,
}

impl Precision {
    /// The name the SQL `precision` argument uses: the vectors' dtype.
    pub fn name(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F64 => "f64",
        }
    }

    /// The bytes of one element.
    pub(super) fn element_bytes(self) -> u64 {
        match self {
            Self::F32 => size_of::<f32>() as u64,
            Self::F64 => size_of::<f64>() as u64,
        }
    }
}

impl FromStr for Precision {
    type Err = SpectralError;

    fn from_str(name: &str) -> Result<Self, SpectralError> {
        [Self::F32, Self::F64]
            .into_iter()
            .find(|precision| precision.name() == name)
            .ok_or_else(|| SpectralError::UnknownPrecision(name.to_owned()))
    }
}

/// What to compute and when to stop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EigenOptions {
    /// The number of eigenpairs.
    pub k: usize,
    /// Which end of the spectrum.
    pub which: Which,
    /// Converged when every wanted pair's residual estimate is at most
    /// `tol · max|θ|`, the largest Ritz value standing in for `‖M‖`.
    pub tol: f64,
    /// The most Lanczos steps (operator applications), raised to `k`;
    /// `None`: 1,000 times the basis size.
    pub max_iter: Option<usize>,
    /// The element type of the Lanczos vectors and the eigenvectors.
    pub precision: Precision,
    /// The seed of the start vector.
    pub seed: u64,
}

impl EigenOptions {
    /// The `k` largest eigenpairs to `tol = 1e-6` (about the rounding of
    /// `f32` vectors, below which their explicit residual does not fall)
    /// with `f32` vectors and seed 0.
    pub fn new(k: usize) -> Self {
        Self {
            k,
            which: Which::Largest,
            tol: 1e-6,
            max_iter: None,
            precision: Precision::F32,
            seed: 0,
        }
    }
}

/// One eigenpair.
#[derive(Debug, PartialEq)]
pub struct Eigenpair {
    /// The eigenvalue.
    pub value: f64,
    /// `‖Mx − λx‖₂` for the unit eigenvector `x` as stored.
    pub residual: f64,
    /// The unit eigenvector, zero at inactive nodes, as a width-1 vector of
    /// the precision's element type.
    pub vector: AnyVector,
}

/// The eigenpairs, in [`Which`] order, and how the iteration ended.
#[derive(Debug, PartialEq)]
pub struct Eigen {
    /// The `k` eigenpairs.
    pub pairs: Vec<Eigenpair>,
    /// The Lanczos steps taken.
    pub iterations: usize,
    /// Whether every pair met the tolerance before `max_iter`.
    pub converged: bool,
    /// The connected components of the active nodes. With more than one,
    /// repeated eigenvalues are likely (each Laplacian has eigenvalue 0 once
    /// per component), and single-vector Lanczos may find fewer copies than
    /// there are.
    pub components: u64,
}

/// The `options.k` eigenpairs of `matrix` over `graph` at the `which` end.
/// Once the operator is prepared, and before the first step, the whole
/// working set is checked against the compute memory, so a solve that
/// cannot fit is refused (`SpectralError::WorkingSet`) without running.
pub fn eigen<I: Interrupts>(
    resources: &mut Resources<'_, I>,
    graph: &GraphCsr,
    matrix: &GraphMatrix<'_>,
    options: &EigenOptions,
) -> Result<Eigen, SpectralError> {
    let mut operator = LinearOperator::new(resources, graph, matrix)?;
    let size = admit(resources, &operator, matrix, options)?;
    match options.precision {
        Precision::F32 => {
            Lanczos::<f32>::new(resources, &mut operator, options, size)?.run(resources)
        }
        Precision::F64 => {
            Lanczos::<f64>::new(resources, &mut operator, options, size)?.run(resources)
        }
    }
}

/// Refuses what [`eigen`] would refuse before its first step, without
/// solving: for a caller about to run several solves in turn (each of
/// which must fit once the one before has released its memory), so that
/// a later one is not refused after an earlier one has run.
pub fn admit_eigen<I: Interrupts>(
    resources: &mut Resources<'_, I>,
    graph: &GraphCsr,
    matrix: &GraphMatrix<'_>,
    options: &EigenOptions,
) -> Result<(), SpectralError> {
    let operator = LinearOperator::new(resources, graph, matrix)?;
    admit(resources, &operator, matrix, options).map(drop)
}

/// Checks the eigenpair count against the dimension and the working set
/// against the compute memory; returns the basis size.
fn admit<I>(
    resources: &Resources<'_, I>,
    operator: &LinearOperator<'_>,
    matrix: &GraphMatrix<'_>,
    options: &EigenOptions,
) -> Result<usize, SpectralError> {
    let dimension = operator.dimension();
    if options.k == 0 || options.k as u64 > dimension {
        return Err(SpectralError::EigenpairCount { k: options.k, dimension });
    }
    let working_set = WorkingSet {
        id_space: operator.id_space(),
        dimension,
        operator: matrix.operator,
        k: options.k,
        precision: options.precision,
    };
    working_set.admit(resources.memory)?;
    Ok(working_set.basis_size())
}

/// One Lanczos iteration in progress.
#[derive(Debug)]
struct Lanczos<'o, 'g, E> {
    operator: &'o mut LinearOperator<'g>,
    options: EigenOptions,
    dimension: usize,
    max_iter: usize,
    basis: Basis<E>,
    /// `QᵀMQ`, `size × size`; the leading `basis.len()` square is in use.
    h: DMatrix<f64>,
    _h_charge: MemoryCharge,
    /// The coupling `β` of the newest basis vector to the one before it,
    /// if the newest came from the plain three-term recurrence.
    previous: Option<f64>,
    /// Random vectors drawn so far.
    streams: u64,
    iterations: usize,
}

/// How a Rayleigh–Ritz step ends the iteration, if it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Converged,
    OutOfSteps,
    /// Restart; this many of the `k` wanted pairs have converged.
    Continue(usize),
}

impl<'o, 'g, E: Stored> Lanczos<'o, 'g, E>
where
    AnyVector: From<Vector<E>>,
{
    fn new<I: Interrupts>(
        resources: &mut Resources<'_, I>,
        operator: &'o mut LinearOperator<'g>,
        options: &EigenOptions,
        size: usize,
    ) -> Result<Self, SpectralError> {
        let dimension = operator.dimension() as usize;
        let max_iter = options.max_iter.unwrap_or(STEPS_PER_BASIS_VECTOR * size).max(options.k);
        let basis = Basis::new(resources, operator.id_space() as usize, size)?;
        let h_charge = resources.memory.charge_elements::<f64>(size * size)?;
        Ok(Self {
            operator,
            options: *options,
            dimension,
            max_iter,
            basis,
            h: DMatrix::zeros(size, size),
            _h_charge: h_charge,
            previous: None,
            streams: 0,
            iterations: 0,
        })
    }

    /// How close two magnitudes may be, relative to the largest, for
    /// `Largest` to treat them as tied: the precision's rounding, never the
    /// tolerance, which would let a loose `tol` reorder distinct values.
    fn tie(&self) -> f64 {
        match self.options.precision {
            Precision::F32 => F32_TIE,
            Precision::F64 => F64_TIE,
        }
    }

    /// The Rayleigh–Ritz pairs of the basis.
    fn ritz(&self, memory: &ComputeMemory) -> Result<Ritz, SpectralError> {
        let len = self.basis.len();
        Ritz::of(memory, &self.h.view((0, 0), (len, len)), self.options.which, self.tie())
    }

    fn size(&self) -> usize {
        self.h.nrows()
    }

    fn run<I: Interrupts>(
        mut self,
        resources: &mut Resources<'_, I>,
    ) -> Result<Eigen, SpectralError> {
        if !self.extend_randomly(resources)? {
            let dimension = self.dimension as u64;
            return Err(SpectralError::EigenpairCount { k: self.options.k, dimension });
        }
        loop {
            resources.interrupts.check().map_err(ComputeError::from)?;
            let beta = self.step(resources)?;
            let len = self.basis.len();
            if len < self.size() && self.iterations < self.max_iter {
                if self.extend(resources, beta)? {
                    continue;
                }
                // Nothing is left to extend the basis with: it spans an
                // invariant subspace holding every active node.
                return self.finish(resources, Outcome::Converged);
            }
            let ritz = self.ritz(resources.memory)?;
            match self.outcome(&ritz, beta.unwrap_or(0.0)) {
                Outcome::Continue(converged) => {
                    self.restart(resources, &ritz, converged)?;
                    if !self.extend(resources, beta)? {
                        // `finish` decomposes again: release this one first,
                        // so the peak stays within `WorkingSet::bytes`.
                        drop(ritz);
                        return self.finish(resources, Outcome::Converged);
                    }
                    // The residual couples to every kept Ritz vector.
                    self.previous = None;
                }
                outcome => return self.finish_with(resources, ritz, outcome),
            }
        }
    }

    /// Multiplies the newest basis vector, makes the result orthogonal to
    /// the basis, and records its coefficients in `H`. Returns the norm of
    /// what is left, `None` on breakdown.
    fn step<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
    ) -> Result<Option<f64>, SpectralError> {
        let j = self.basis.len() - 1;
        self.basis.column(resources, j, self.operator.input_mut())?;
        let w = self.operator.apply(resources)?;
        self.iterations += 1;
        let Orthogonalized { coefficients, norm: beta } =
            self.basis.orthogonalize(resources, w, self.previous)?;
        for (i, &c) in coefficients.iter().enumerate() {
            self.h[(i, j)] = c;
            self.h[(j, i)] = c;
        }
        let before = (coefficients.iter().map(|c| c * c).sum::<f64>() + beta * beta).sqrt();
        if !beta.is_finite() {
            return Err(SpectralError::NotFinite);
        }
        Ok((beta > BREAKDOWN * before).then_some(beta))
    }

    /// Appends the last residual, normalised, or after a breakdown a fresh
    /// random vector. False if there is no direction left.
    fn extend<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
        beta: Option<f64>,
    ) -> Result<bool, SpectralError> {
        match beta {
            Some(beta) => {
                let w = self.operator.output_mut();
                self.basis.push(resources, w, beta)?;
                self.previous = Some(beta);
                Ok(true)
            }
            None => self.extend_randomly(resources),
        }
    }

    /// Appends a fresh random vector orthogonal to the basis.
    fn extend_randomly<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
    ) -> Result<bool, SpectralError> {
        self.streams += 1;
        let v = self.operator.random_input(resources, self.options.seed, self.streams - 1)?;
        let before = dense::sum(resources, v.len(), |i| v[i] * v[i])?.sqrt();
        let norm = self.basis.orthogonalize(resources, v, None)?.norm;
        if norm <= BREAKDOWN * before {
            return Ok(false);
        }
        self.basis.push(resources, v, norm)?;
        self.previous = None;
        Ok(true)
    }

    /// Whether the `k` wanted Ritz pairs have converged, given the norm of
    /// the residual left after the last basis vector.
    fn outcome(&self, ritz: &Ritz, beta: f64) -> Outcome {
        let len = self.basis.len();
        let scale = ritz.values.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let converged = (0..self.options.k)
            .filter(|&l| (beta * ritz.vectors[(len - 1, l)]).abs() <= self.options.tol * scale)
            .count();
        let done = len == self.dimension || converged == self.options.k;
        match (done, self.iterations >= self.max_iter) {
            (true, _) => Outcome::Converged,
            (false, true) => Outcome::OutOfSteps,
            (false, false) => Outcome::Continue(converged),
        }
    }

    /// Compresses the basis to its most wanted Ritz vectors, on which `H` is
    /// diagonal: the `k` wanted ones plus one more for each that has
    /// converged (ARPACK's adjustment, so converged pairs do not crowd out
    /// the ones still converging), at least a quarter and at most half of
    /// the rest of the basis (so a clustered spectrum keeps enough of its
    /// Krylov space).
    fn restart<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
        ritz: &Ritz,
        converged: usize,
    ) -> Result<(), SpectralError> {
        let k = self.options.k;
        let free = self.size() - k;
        let kept = (k + converged.clamp(free / 4, free / 2)).min(self.size() - 1);
        self.basis.rotate(resources, &ritz.vectors.columns(0, kept).into_owned())?;
        self.h.fill(0.0);
        for (l, &value) in ritz.values[..kept].iter().enumerate() {
            self.h[(l, l)] = value;
        }
        Ok(())
    }

    /// Ends the iteration with the Ritz pairs of the current basis.
    fn finish<I: Interrupts>(
        self,
        resources: &mut Resources<'_, I>,
        outcome: Outcome,
    ) -> Result<Eigen, SpectralError> {
        let ritz = self.ritz(resources.memory)?;
        self.finish_with(resources, ritz, outcome)
    }

    /// The `k` wanted eigenpairs from `ritz`.
    fn finish_with<I: Interrupts>(
        self,
        resources: &mut Resources<'_, I>,
        ritz: Ritz,
        outcome: Outcome,
    ) -> Result<Eigen, SpectralError> {
        let mut pairs = Vec::new();
        for l in 0..self.options.k {
            let coordinates: Vec<f64> = ritz.vectors.column(l).iter().copied().collect();
            self.basis.combine(resources, &coordinates, self.operator.input_mut())?;
            settle::<E, I>(resources, self.operator.input_mut())?;
            let value = ritz.values[l];
            let residual = self.operator.residual(resources, value)?;
            if self.operator.map_eigenvector(resources)? {
                settle::<E, I>(resources, self.operator.input_mut())?;
            }
            let x = self.operator.input_mut();
            let mut vector = Vector::filled(resources.memory, x.len() as u64, 1, E::default())?;
            for (stored, &value) in vector.values_mut().iter_mut().zip(x.iter()) {
                *stored = E::from_f64(value);
            }
            pairs.push(Eigenpair { value, residual, vector: vector.into() });
        }
        Ok(Eigen {
            pairs,
            iterations: self.iterations,
            converged: outcome == Outcome::Converged,
            components: self.operator.components(resources)?,
        })
    }
}

/// Normalises `x`, flips it so that its entry of largest magnitude (the
/// first, on ties) is positive, and rounds it to the stored precision.
fn settle<E: Stored, I: Interrupts>(
    resources: &mut Resources<'_, I>,
    x: &mut [f64],
) -> Result<(), SpectralError> {
    let norm = dense::sum(resources, x.len(), |i| x[i] * x[i])?.sqrt();
    let top = x.iter().fold((0.0f64, 1.0f64), |(m, sign), &v| match v.abs() > m {
        true => (v.abs(), v.signum()),
        false => (m, sign),
    });
    let factor = top.1 / norm;
    dense::update(resources, x, |_, v| E::from_f64(v * factor).to_f64())?;
    Ok(())
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests;
