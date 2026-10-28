//! PageRank by power iteration, as NetworkX computes it (DESIGN.md §6.7):
//! rank flows along edges, split by out-weight; the rank of dangling
//! nodes (no out-weight above zero), and the `1 − damping` teleport, go to
//! the personalisation, uniform or proportional to a seed vector; it
//! starts uniform and stops once an iteration's L1 change falls below live
//! nodes × `tol`, or after `max_iter` iterations.
//!
//! Through 0.7.0 this was the PL/pgSQL library's loop over the compute
//! primitives. The kernel computes exactly what that loop did, to the bit
//! and with its errors: the same products (`multiply_into`, the kernel
//! `spmv` runs), its sums in the chunks and order `vector_reduce` and
//! `vector_norm_diff` use, and each step's arithmetic as SQL's `float8`
//! operators do it (`map::float`, which `vector_map` evaluates with), in
//! two parallel passes an iteration instead of eleven calls.

use rayon::prelude::*;

use super::map::float;
use super::reduce::{reduce_live, Reduction, CHUNK};
use super::{
    multiply_into, AnyVector, Arithmetic, ComputeError, ComputeMemory, Dtype, Element, Matrix,
    MemoryCharge, Product, Semiring, Vector, Workers,
};
use crate::graph::{GraphCsr, RelationSelection, Tombstones};
use crate::interrupts::{Cancellation, Cancelled, Interrupts};

/// What PageRank ranks by, and when it stops.
#[derive(Debug, Clone, Copy)]
pub struct PageRank<'a> {
    /// The relations whose edges rank flows along.
    pub relations: RelationSelection<'a>,
    /// Use edge weights (`None`: if the graph has them).
    pub weighted: Option<bool>,
    /// Read a pair linked in several relations as an edge in each.
    pub per_relation: bool,
    /// The probability of following an edge rather than teleporting.
    pub damping: f64,
    /// Stop once an iteration changes the ranks by less than live nodes ×
    /// `tol` in L1, compared in SQL order (so a `NaN` `tol` stops after
    /// one); `None`, never.
    pub tol: Option<f64>,
    /// The most iterations to run.
    pub max_iter: u32,
    /// The personalisation, proportional to this numeric width-1 vector;
    /// `None`, uniform.
    pub seed: Option<&'a AnyVector>,
}

/// The ranks, and the iterations run.
#[derive(Debug)]
pub struct Ranks {
    /// Each node's rank; zero at tombstoned rows.
    pub values: Vector<f64>,
    /// The iterations run.
    pub iterations: u32,
}

/// The constants of one iteration's update, `next = damping · (y +
/// dangling · p) + teleport · p` at each live row.
#[derive(Debug, Clone, Copy)]
struct Update {
    damping: f64,
    teleport: f64,
    dangling: f64,
}

/// Where teleported (and dangling) rank goes.
#[derive(Debug)]
enum Personalisation {
    /// Every live node alike: `1 / live nodes`.
    Uniform(f64),
    /// `seed / sum(seed)` at each live row.
    Seeded { shares: Vec<f64>, _charge: MemoryCharge },
}

impl Personalisation {
    #[inline(always)]
    fn at(&self, row: usize) -> f64 {
        match self {
            Self::Uniform(share) => *share,
            Self::Seeded { shares, .. } => shares[row],
        }
    }
}

/// PageRank over `graph`'s live nodes (those `tombstones` does not mark),
/// on `workers`, its buffers charged to `memory`: four vectors of the id
/// space, a fifth with a seed, and a product's scratch where the graph has
/// no reverse CSR. Reports its iterations through `interrupts`' progress
/// (phase `iterating`, of `max_iter`) as each starts.
///
/// Refused as SQL refuses the arithmetic (division by zero where no node
/// is live or the seed sums to zero; a float overflow or underflow in a
/// step), a seed that is `bool` ([`ComputeError::DtypeMismatch`]) or, once
/// an iteration runs, wider than one ([`ComputeError::WidthMismatch`]).
pub fn pagerank<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &GraphCsr,
    tombstones: &Tombstones,
    pagerank: &PageRank<'_>,
) -> Result<Ranks, ComputeError> {
    let id_space = graph.all_relations().forward().id_space();
    let live = (0..id_space).filter(|&node| !tombstones.contains(node)).count();
    let nodes = live as f64;
    let start = float(Arithmetic::Divide, 1.0, nodes)?;
    let matrix = |transpose| {
        let PageRank { relations, weighted, per_relation, .. } = *pagerank;
        Matrix::new(graph, relations, transpose, weighted, per_relation, memory)
    };
    let (forward, transposed) = (matrix(false)?, matrix(true)?);
    let mut x = Vector::filled(memory, id_space, 1, start)?;
    x.clear_tombstoned(tombstones)?;
    let (mut out, _out) = memory.filled(id_space as usize, 0.0)?;
    let out_weight = Product::new(Semiring::PlusFirst);
    multiply_into(workers, memory, interrupts, &forward, &out_weight, &x, &mut out)?;
    let personalisation = match pagerank.seed {
        None => Personalisation::Uniform(start),
        Some(seed) => seeded(workers, memory, interrupts, tombstones, seed, pagerank.max_iter)?,
    };
    let teleport = float(Arithmetic::Subtract, 1.0, pagerank.damping)?;
    let mut msg = Vector::filled(memory, id_space, 1, 0.0)?;
    let (mut y, _y) = memory.filled(id_space as usize, 0.0)?;
    let inflow = Product::new(Semiring::PlusTimes);
    let mut iterations = 0;
    while iterations < pagerank.max_iter {
        iterations += 1;
        if let Some(progress) = interrupts.progress() {
            if iterations == 1 {
                progress.phase("iterating", Some(u64::from(pagerank.max_iter)));
            }
            progress.done(u64::from(iterations - 1));
        }
        let dangling = workers.run(interrupts, |cancellation| {
            spread(cancellation, x.values(), &out, msg.values_mut()?)
        })?;
        multiply_into(workers, memory, interrupts, &transposed, &inflow, &msg, &mut y)?;
        let update = Update { damping: pagerank.damping, teleport, dangling };
        let delta = workers.run(interrupts, |cancellation| {
            let step = Step { y: &y, personalisation: &personalisation, tombstones, update };
            advance(cancellation, &step, x.values_mut()?)
        })?;
        if let Some(tol) = pagerank.tol {
            if delta.sql_cmp(float(Arithmetic::Multiply, nodes, tol)?).is_lt() {
                break;
            }
        }
    }
    Ok(Ranks { values: x, iterations })
}

/// The personalisation `seed / sum(seed)`, its sum over the live rows as
/// `vector_reduce` sums, each share as `float8` multiplies.
fn seeded<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    tombstones: &Tombstones,
    seed: &AnyVector,
    max_iter: u32,
) -> Result<Personalisation, ComputeError> {
    let sum = reduce_live(workers, interrupts, seed, tombstones, Reduction::Sum)?;
    let total = float(Arithmetic::Divide, 1.0, sum.unwrap_or(0.0))?;
    let shares = match seed {
        AnyVector::F64(seed) => shares(memory, tombstones, seed, total, max_iter),
        AnyVector::F32(seed) => shares(memory, tombstones, seed, total, max_iter),
        AnyVector::I64(seed) => shares(memory, tombstones, seed, total, max_iter),
        AnyVector::Bool(_) => {
            Err(ComputeError::DtypeMismatch { expected: Dtype::F64, found: Dtype::Bool })
        }
    }?;
    Ok(shares)
}

/// `seed × total` at each live row, zero at tombstoned ones. A seed of
/// several columns fails as the PL/pgSQL library failed, in its first
/// iteration, where a width-1 vector met it: so not without one.
fn shares<E: Element>(
    memory: &ComputeMemory,
    tombstones: &Tombstones,
    seed: &Vector<E>,
    total: f64,
    max_iter: u32,
) -> Result<Personalisation, ComputeError> {
    if seed.width() != 1 {
        return match max_iter {
            0 => Ok(Personalisation::Uniform(0.0)),
            _ => Err(ComputeError::WidthMismatch { expected: seed.width(), found: 1 }),
        };
    }
    let (mut shares, charge) = memory.filled(seed.values().len(), 0.0)?;
    for (row, (share, value)) in (0..).zip(shares.iter_mut().zip(seed.values())) {
        if !tombstones.contains(row) {
            *share = float(Arithmetic::Multiply, value.to_f64(), total)?;
        }
    }
    Ok(Personalisation::Seeded { shares, _charge: charge })
}

/// The messages `msg = x / out` (0 where `out` is not above 0, in SQL
/// order), returning the dangling rank: the sum of `x` where it is not,
/// summed as `vector_reduce` sums, in chunks combined in order.
fn spread(
    cancellation: &Cancellation,
    x: &[f64],
    out: &[f64],
    msg: &mut [f64],
) -> Result<f64, ComputeError> {
    let partials: Vec<Result<f64, ComputeError>> = msg
        .par_chunks_mut(CHUNK)
        .enumerate()
        .map(|(chunk, msg)| {
            if cancellation.is_cancelled() {
                return Err(Cancelled.into());
            }
            let start = chunk * CHUNK;
            let mut dangling = 0.0;
            for (msg, (&x, &out)) in msg.iter_mut().zip(x[start..].iter().zip(&out[start..])) {
                *msg = match out.sql_cmp(0.0).is_gt() {
                    true => float(Arithmetic::Divide, x, out)?,
                    false => {
                        dangling += x;
                        0.0
                    }
                };
            }
            Ok(dangling)
        })
        .collect();
    total(partials)
}

/// What one iteration's update reads.
struct Step<'s> {
    y: &'s [f64],
    personalisation: &'s Personalisation,
    tombstones: &'s Tombstones,
    update: Update,
}

/// `x ← damping · (y + dangling · p) + teleport · p` at each live row (0
/// at tombstoned ones), returning the L1 change, summed as
/// `vector_norm_diff` sums it.
fn advance(
    cancellation: &Cancellation,
    step: &Step<'_>,
    x: &mut [f64],
) -> Result<f64, ComputeError> {
    let Update { damping, teleport, dangling } = step.update;
    let partials: Vec<Result<f64, ComputeError>> = x
        .par_chunks_mut(CHUNK)
        .enumerate()
        .map(|(chunk, x)| {
            if cancellation.is_cancelled() {
                return Err(Cancelled.into());
            }
            let start = chunk * CHUNK;
            let mut change = 0.0;
            for (offset, (x, &y)) in x.iter_mut().zip(&step.y[start..]).enumerate() {
                let row = start + offset;
                let next = match step.tombstones.contains(row as u64) {
                    true => 0.0,
                    false => {
                        let p = step.personalisation.at(row);
                        let flowed =
                            float(Arithmetic::Add, y, float(Arithmetic::Multiply, dangling, p)?)?;
                        float(
                            Arithmetic::Add,
                            float(Arithmetic::Multiply, damping, flowed)?,
                            float(Arithmetic::Multiply, teleport, p)?,
                        )?
                    }
                };
                change += (next - *x).abs();
                *x = next;
            }
            Ok(change)
        })
        .collect();
    total(partials)
}

/// The chunks' sums added in chunk order, or the first chunk's error.
fn total(partials: Vec<Result<f64, ComputeError>>) -> Result<f64, ComputeError> {
    partials.into_iter().try_fold(0.0, |sum, partial| Ok(sum + partial?))
}

// Not under Miri: the compute pool's work stealing runs through
// crossbeam-epoch, which Miri's Stacked Borrows model rejects (a known
// report against crossbeam-epoch 0.9, not this crate, which has no unsafe).
#[cfg(test)]
#[cfg(not(miri))]
mod tests;
