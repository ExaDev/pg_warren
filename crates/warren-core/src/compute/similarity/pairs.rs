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

/// The `measure` of each pair of `pairs` (dense ids of live nodes).
/// Neighbourhoods are read node by node when every selected relation has
/// a reverse CSR, else from the symmetric adjacency built once.
pub fn similarity_of_pairs<I: Interrupts>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    graph: &Neighbourhood<'_, '_>,
    measure: Similarity,
    pairs: &[(DenseId, DenseId)],
) -> Result<Scores, ComputeError> {
    let (mut values, charge) = memory.filled(pairs.len(), 0.0)?;
    let local = Neighbourhoods::local(graph.graph, graph.relations)?;
    if local.is_none() {
        memory.reclaim_for(Neighbourhoods::built_bytes(graph.graph, graph.relations)?);
    }
    workers.run(interrupts, |cancellation| {
        let neighbourhoods = match local {
            Some(local) => local,
            None => Neighbourhoods::built(memory, cancellation, graph.graph, graph.relations)?,
        };
        score_pairs(memory, cancellation, &neighbourhoods, measure, pairs, &mut values)
    })?;
    Ok(Scores { values, _charge: charge })
}

fn score_pairs(
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
        let (mut a, mut b) = (RowBuffer::new(memory)?, RowBuffer::new(memory)?);
        let (mut degrees, mut commons) = (RowBuffer::new(memory)?, RowBuffer::new(memory)?);
        for (&(u, v), score) in pairs.iter().zip(scores) {
            let row_u = neighbourhoods.row(u, &mut a, &mut work)?;
            let row_v = neighbourhoods.row(v, &mut b, &mut work)?;
            let pair_degrees =
                (neighbourhoods.degree_of_row(u, row_u), neighbourhoods.degree_of_row(v, row_v));
            let mut found = Shared::default();
            if measure.weighs_neighbours() {
                let common = commons.collect(row_u.len().min(row_v.len()), |each| {
                    shared(row_u, row_v, &mut work, each).map(|_| ())
                })?;
                for &w in common {
                    found.count += 1;
                    found.adamic_adar +=
                        adamic_adar_term(neighbourhoods.degree(w, &mut degrees, &mut work)?);
                }
            } else {
                found.count = shared(row_u, row_v, &mut work, |_| ())?;
            }
            *score = measure.score(found, pair_degrees);
        }
        Ok(())
    })
}
