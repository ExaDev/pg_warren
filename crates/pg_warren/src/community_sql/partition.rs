//! What the partition functions (`modularity`, `local_moving`,
//! `leiden_refine`) read of a version (DESIGN.md §6.7): its undirected
//! weighted view, built on the compute pool within the call's budget, and
//! label vectors as integers.

use warren_core::community::{Labels, Undirected};
use warren_core::compute::MemoryCharge;
use warren_core::graph::BuildMemory;

use super::CommunitySqlError;
use crate::compute_sql::{workers, Vectors};
use crate::interrupts::PostgresInterrupts;
use crate::source::IdDatum;

/// The undirected weighted view of `vectors`' version: each edge weighs
/// its weight if `weighted` (NULL: if the graph has weights), else 1; the
/// two directions of a directed graph add up.
pub(super) fn undirected<E: IdDatum>(
    vectors: &Vectors<E>,
    weighted: Option<bool>,
) -> Result<Undirected, CommunitySqlError> {
    let weighted = weighted.unwrap_or(vectors.graph.weighted);
    let directed = vectors.graph.directed;
    let graph = &vectors.loaded.graph;
    vectors.memory.reclaim_for(Undirected::bytes(graph, directed));
    let memory = &vectors.memory;
    let built = workers()?.run(&mut PostgresInterrupts, |cancellation| {
        Ok(Undirected::new(memory, cancellation, graph, weighted, directed))
    })?;
    Ok(built?)
}

/// Vector `name` of `vectors`' version as one integer per node (0 for a
/// tombstoned node), refused as `contract` refuses labels that are not
/// integers, with its charge to the call's budget. `what` names the
/// argument, for the error.
pub(super) fn integers<E: IdDatum>(
    vectors: &mut Vectors<E>,
    name: &str,
    what: &'static str,
) -> Result<(Labels, MemoryCharge), CommunitySqlError> {
    let vector = vectors.get(name)?;
    let charge = vectors.memory.charge(vectors.id_space().saturating_mul(8))?;
    let labels = Labels::read(&vector, vectors.tombstones(), &BuildMemory::unlimited())
        .map_err(|error| CommunitySqlError::Integers { what, error })?;
    Ok((labels, charge))
}
