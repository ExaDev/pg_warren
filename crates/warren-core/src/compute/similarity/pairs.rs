//! Similarity of given pairs (NetworkX's `ebunch`): each pair's rows
//! intersected as `triangles` intersects them, merged or, for a hub,
//! binary-searched, in parallel tasks of pairs.

use rayon::prelude::*;

use super::super::symmetric::shared;
use super::super::{ComputeError, ComputeMemory, MemoryCharge, Workers};
use super::neighbourhoods::{Neighbourhoods, RowBuffer};
use super::{adamic_adar_term, Neighbourhood, Shared, Similarity};
use crate::graph::DenseId;
use crate::interrupts::{Cancellation, Interrupts, Work};

/// The pairs one task scores; a task checks the cancellation flag first.
const PAIRS_PER_TASK: usize = if cfg!(test) { 3 } else { 4096 };

/// Scores, one per pair, charged to the compute budget.
#[derive(Debug)]
pub struct Scores {
    /// The scores, in the pairs' order.
    pub values: Vec<f64>,
    _charge: MemoryCharge,
}

/// The `measure` of each pair of `pairs` (dense ids of live nodes). When
/// every selected relation has a reverse CSR, neighbourhoods are read node
/// by node, on the calling thread (so what the rows charge reclaims held
/// memory as it needs it); else the symmetric adjacency is built once and
/// the pairs are scored on the pool, reading rows in place.
pub fn similarity_of_pairs<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &Neighbourhood<'_, '_>,
    measure: Similarity,
    pairs: &[(DenseId, DenseId)],
) -> Result<Scores, ComputeError> {
    let (mut values, charge) = memory.filled(pairs.len(), 0.0)?;
    match Neighbourhoods::local(graph.graph, graph.relations)? {
        Some(local) => {
            let mut buffers = Buffers::new(memory)?;
            let mut work = Work::polling(interrupts);
            for (&pair, score) in pairs.iter().zip(&mut values) {
                *score = score_pair(&local, measure, pair, &mut buffers, &mut work)?;
            }
        }
        None => {
            memory.reclaim_for(Neighbourhoods::built_bytes(graph.graph, graph.relations)?);
            workers.run(interrupts, |cancellation| {
                let built =
                    Neighbourhoods::built(memory, cancellation, graph.graph, graph.relations)?;
                score_built(memory, cancellation, &built, measure, pairs, &mut values)
            })?;
        }
    }
    Ok(Scores { values, _charge: charge })
}

/// The rows one scorer reads node by node into.
struct Buffers {
    u: RowBuffer,
    v: RowBuffer,
    degrees: RowBuffer,
    common: RowBuffer,
}

impl Buffers {
    fn new(memory: &ComputeMemory) -> Result<Self, ComputeError> {
        Ok(Self {
            u: RowBuffer::new(memory)?,
            v: RowBuffer::new(memory)?,
            degrees: RowBuffer::new(memory)?,
            common: RowBuffer::new(memory)?,
        })
    }
}

/// Scores `pairs` on the pool over the built adjacency, whose rows and
/// degrees are read in place: the buffers never grow.
fn score_built(
    memory: &ComputeMemory,
    cancellation: &Cancellation,
    neighbourhoods: &Neighbourhoods<'_>,
    measure: Similarity,
    pairs: &[(DenseId, DenseId)],
    scores: &mut [f64],
) -> Result<(), ComputeError> {
    let tasks = pairs.par_chunks(PAIRS_PER_TASK).zip(scores.par_chunks_mut(PAIRS_PER_TASK));
    tasks.try_for_each(|(pairs, scores)| {
        let mut work = Work::new(cancellation);
        work.check()?;
        let mut buffers = Buffers::new(memory)?;
        for (&pair, score) in pairs.iter().zip(scores) {
            *score = score_pair(neighbourhoods, measure, pair, &mut buffers, &mut work)?;
        }
        Ok(())
    })
}

/// The `measure` of the pair `(u, v)`.
fn score_pair(
    neighbourhoods: &Neighbourhoods<'_>,
    measure: Similarity,
    (u, v): (DenseId, DenseId),
    buffers: &mut Buffers,
    work: &mut Work<'_>,
) -> Result<f64, ComputeError> {
    let row_u = neighbourhoods.row(u, &mut buffers.u, work)?;
    let row_v = neighbourhoods.row(v, &mut buffers.v, work)?;
    let degrees = (neighbourhoods.degree_of_row(u, row_u), neighbourhoods.degree_of_row(v, row_v));
    let mut found = Shared::default();
    if !measure.weighs_neighbours() {
        found.count = shared(row_u, row_v, work, |_| ())?;
    } else if let Some(known) = neighbourhoods.degrees() {
        found.count = shared(row_u, row_v, work, |w| {
            found.adamic_adar += adamic_adar_term(known[w as usize]);
        })?;
    } else {
        let common = buffers.common.collect(row_u.len().min(row_v.len()), |each| {
            shared(row_u, row_v, work, each).map(|_| ())
        })?;
        for &w in common {
            found.count += 1;
            let degree = neighbourhoods.degree(w, &mut buffers.degrees, work)?;
            found.adamic_adar += adamic_adar_term(degree);
        }
    }
    Ok(measure.score(found, degrees))
}
