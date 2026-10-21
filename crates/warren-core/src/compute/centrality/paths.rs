//! Single-source shortest paths, one source at a time in a reusable
//! scratch: breadth-first search on unweighted rows, Dijkstra's algorithm
//! on weighted ones (positive, finite weights only), with each reached
//! node's distance and, for betweenness, its number of shortest paths
//! (Brandes 2001) and its dependency on the source.
//!
//! The scratch is a dense array per quantity over the id space; a search
//! touches and resets only the nodes it reaches, so a source costs
//! `O(N_s + M_s)` (times `log N_s` weighted) over what it reaches.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::mem::size_of;

use super::super::product::RowScratch;
use super::super::{ComputeError, ComputeMemory, MemoryCharge};
use super::adjacency::Adjacency;
use crate::graph::DenseId;
use crate::interrupts::Work;

/// One search's results and scratch.
pub(super) struct Paths {
    source: DenseId,
    /// Each node's distance from the source; infinite if not reached.
    distance: Vec<f64>,
    /// Each node's number of shortest paths from the source (betweenness
    /// only: else empty).
    counts: Vec<f64>,
    /// Each node's dependency on the source, δ (betweenness only).
    dependency: Vec<f64>,
    /// The nodes reached, by distance from the source.
    order: Vec<DenseId>,
    /// Dijkstra's queue of tentative distances, grown (and charged) on the
    /// pool.
    queue: Queue,
    queue_charge: MemoryCharge,
    scratch: RowScratch,
    _charge: MemoryCharge,
}

/// A tentative distance, ordered by `f64::total_cmp`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Tentative(f64);

impl Eq for Tentative {}

impl PartialOrd for Tentative {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Tentative {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl Paths {
    /// The bytes [`Self::new`] charges for an id space, counting paths
    /// and dependencies if `counted`.
    pub(super) fn bytes(id_space: u64, counted: bool) -> u64 {
        let arrays = if counted { 4 } else { 2 };
        id_space.saturating_mul(arrays * size_of::<f64>() as u64)
    }

    /// An empty scratch over `id_space` nodes.
    pub(super) fn new(
        memory: &ComputeMemory,
        id_space: u64,
        counted: bool,
    ) -> Result<Self, ComputeError> {
        let charge = memory.charge(Self::bytes(id_space, counted))?;
        let filled = |len: u64, value| {
            let mut buffer = Vec::new();
            buffer
                .try_reserve_exact(len as usize)
                .map_err(|_| ComputeError::OutOfMemory { bytes: charge.bytes() })?;
            buffer.resize(len as usize, value);
            Ok::<_, ComputeError>(buffer)
        };
        let counted_len = if counted { id_space } else { 0 };
        let mut order = Vec::new();
        order
            .try_reserve_exact(id_space as usize)
            .map_err(|_| ComputeError::OutOfMemory { bytes: charge.bytes() })?;
        Ok(Self {
            source: 0,
            distance: filled(id_space, f64::INFINITY)?,
            counts: filled(counted_len, 0.0)?,
            dependency: filled(counted_len, 0.0)?,
            order,
            queue: BinaryHeap::new(),
            queue_charge: memory.charge(0)?,
            scratch: RowScratch::new(memory)?,
            _charge: charge,
        })
    }

    /// The source of the last search.
    pub(super) fn source(&self) -> DenseId {
        self.source
    }

    /// Each node's distance from the source (infinite if not reached).
    pub(super) fn distance(&self) -> &[f64] {
        &self.distance
    }

    /// Each node's dependency on the source (betweenness only).
    pub(super) fn dependency(&self) -> &[f64] {
        &self.dependency
    }

    /// The nodes reached, and their dependencies on the source to turn
    /// into what each node's betweenness gains.
    pub(super) fn reached_and_dependency(&mut self) -> (&[DenseId], &mut [f64]) {
        (&self.order, &mut self.dependency)
    }

    /// Forgets the last search, touching only the nodes it reached.
    fn reset(&mut self) {
        let counted = !self.counts.is_empty();
        for &node in &self.order {
            let node = node as usize;
            self.distance[node] = f64::INFINITY;
            if counted {
                self.counts[node] = 0.0;
                self.dependency[node] = 0.0;
            }
        }
        self.order.clear();
    }

    /// The shortest paths from `source` over `adjacency`.
    pub(super) fn search(
        &mut self,
        adjacency: &Adjacency<'_>,
        source: DenseId,
        work: &mut Work<'_>,
    ) -> Result<(), ComputeError> {
        self.reset();
        self.source = source;
        self.distance[source as usize] = 0.0;
        if !self.counts.is_empty() {
            self.counts[source as usize] = 1.0;
        }
        match adjacency.is_weighted() {
            false => self.breadth_first(adjacency, work),
            true => self.dijkstra(adjacency, work),
        }
    }

    fn breadth_first(
        &mut self,
        adjacency: &Adjacency<'_>,
        work: &mut Work<'_>,
    ) -> Result<(), ComputeError> {
        self.order.push(self.source);
        let mut next = 0;
        while let Some(&node) = self.order.get(next) {
            next += 1;
            work.spend(1)?;
            let (distance, counts, order) = (&mut self.distance, &mut self.counts, &mut self.order);
            let through = distance[node as usize] + 1.0;
            adjacency.for_each(node, &mut self.scratch, work, |target, _| {
                let target = target as usize;
                if distance[target] == f64::INFINITY {
                    distance[target] = through;
                    order.push(target as DenseId);
                }
                if distance[target] == through && !counts.is_empty() {
                    counts[target] += counts[node as usize];
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    fn dijkstra(
        &mut self,
        adjacency: &Adjacency<'_>,
        work: &mut Work<'_>,
    ) -> Result<(), ComputeError> {
        self.queue.clear();
        push(&mut self.queue, &mut self.queue_charge, 0.0, self.source)?;
        while let Some(Reverse((Tentative(at), node))) = self.queue.pop() {
            work.spend(1)?;
            if at > self.distance[node as usize] {
                continue;
            }
            self.order.push(node);
            let (distance, counts) = (&mut self.distance, &mut self.counts);
            let (queue, queue_charge) = (&mut self.queue, &mut self.queue_charge);
            adjacency.for_each(node, &mut self.scratch, work, |target, weight| {
                if !(weight > 0.0 && weight.is_finite()) {
                    return Err(ComputeError::NonPositiveWeight);
                }
                let through = at + weight;
                let t = target as usize;
                if through < distance[t] {
                    distance[t] = through;
                    if !counts.is_empty() {
                        counts[t] = counts[node as usize];
                    }
                    push(queue, queue_charge, through, target)?;
                } else if through == distance[t] && !counts.is_empty() {
                    counts[t] += counts[node as usize];
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    /// Each reached node's dependency on the source (Brandes'
    /// accumulation): over the reached nodes farthest first,
    /// `δ(v) = Σ_w σ(v) / σ(w) · (1 + δ(w))` over the edges `v → w` that
    /// lie on a shortest path (`d(v) + weight = d(w)`).
    pub(super) fn accumulate(
        &mut self,
        adjacency: &Adjacency<'_>,
        work: &mut Work<'_>,
    ) -> Result<(), ComputeError> {
        let (distance, counts, dependency) = (&self.distance, &self.counts, &mut self.dependency);
        for &node in self.order.iter().rev() {
            work.spend(1)?;
            let v = node as usize;
            let mut gained = 0.0;
            adjacency.for_each(node, &mut self.scratch, work, |target, weight| {
                let w = target as usize;
                if distance[v] + weight == distance[w] {
                    gained += counts[v] / counts[w] * (1.0 + dependency[w]);
                }
                Ok(())
            })?;
            dependency[v] = gained;
        }
        Ok(())
    }
}

/// Dijkstra's queue.
type Queue = BinaryHeap<Reverse<(Tentative, DenseId)>>;

/// Queues `node` at tentative distance `at`, charging the queue's growth.
fn push(
    queue: &mut Queue,
    charge: &mut MemoryCharge,
    at: f64,
    node: DenseId,
) -> Result<(), ComputeError> {
    if queue.len() == queue.capacity() {
        let more = queue.capacity().max(64);
        let bytes = (more * size_of::<Reverse<(Tentative, DenseId)>>()) as u64;
        charge.grow(bytes)?;
        queue.try_reserve_exact(more).map_err(|_| {
            charge.shrink(bytes);
            ComputeError::OutOfMemory { bytes }
        })?;
    }
    queue.push(Reverse((Tentative(at), node)));
    Ok(())
}
