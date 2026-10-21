//! Single-source shortest paths, one source at a time in a reusable
//! scratch: breadth-first search on unweighted rows, Dijkstra's algorithm
//! on weighted ones (positive, finite weights only), with each reached
//! node's distance and, for betweenness, its number of shortest paths
//! (Brandes 2001) and its dependency on the source.
//!
//! The scratch is a dense array per quantity over the id space (and, for
//! Dijkstra, an indexed heap of at most one entry per node), all allocated
//! before the search, so nothing grows on the pool; a search touches and
//! resets only the nodes it reaches, so a source costs `O(N_s + M_s)`
//! (times `log N_s` weighted) over what it reaches.

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
    /// Dijkstra's queue of reached, unsettled nodes (weighted only: else
    /// empty).
    queue: Queue,
    scratch: RowScratch,
    _charge: MemoryCharge,
}

impl Paths {
    /// The bytes [`Self::new`] charges for an id space, counting paths
    /// and dependencies if `counted`, with Dijkstra's queue if `weighted`.
    pub(super) fn bytes(id_space: u64, counted: bool, weighted: bool) -> u64 {
        let arrays = 2 + 2 * u64::from(counted) + 2 * u64::from(weighted);
        id_space.saturating_mul(arrays * size_of::<f64>() as u64)
    }

    /// An empty scratch over `id_space` nodes.
    pub(super) fn new(
        memory: &ComputeMemory,
        id_space: u64,
        counted: bool,
        weighted: bool,
    ) -> Result<Self, ComputeError> {
        let charge = memory.charge(Self::bytes(id_space, counted, weighted))?;
        fn filled<T: Clone>(len: u64, value: T, bytes: u64) -> Result<Vec<T>, ComputeError> {
            let mut buffer = Vec::new();
            buffer
                .try_reserve_exact(len as usize)
                .map_err(|_| ComputeError::OutOfMemory { bytes })?;
            buffer.resize(len as usize, value);
            Ok(buffer)
        }
        let bytes = charge.bytes();
        let counted_len = if counted { id_space } else { 0 };
        let reserved = |len: u64| -> Result<Vec<DenseId>, ComputeError> {
            let mut buffer = Vec::new();
            buffer
                .try_reserve_exact(len as usize)
                .map_err(|_| ComputeError::OutOfMemory { bytes })?;
            Ok(buffer)
        };
        let queued_len = if weighted { id_space } else { 0 };
        let queue =
            Queue { heap: reserved(queued_len)?, at: filled(queued_len, NOT_QUEUED, bytes)? };
        Ok(Self {
            source: 0,
            distance: filled(id_space, f64::INFINITY, bytes)?,
            counts: filled(counted_len, 0.0, bytes)?,
            dependency: filled(counted_len, 0.0, bytes)?,
            order: reserved(id_space)?,
            queue,
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
        self.queue.push(&self.distance, self.source);
        while let Some(node) = self.queue.pop(&self.distance) {
            work.spend(1)?;
            self.order.push(node);
            let at = self.distance[node as usize];
            let (distance, counts, queue) = (&mut self.distance, &mut self.counts, &mut self.queue);
            adjacency.for_each(node, &mut self.scratch, work, |target, weight| {
                if target == node {
                    return Ok(());
                }
                let through = at + weight;
                if !(weight > 0.0 && through.is_finite()) {
                    return Err(ComputeError::NonPositiveWeight);
                }
                let t = target as usize;
                if through < distance[t] {
                    let reached = distance[t].is_finite();
                    distance[t] = through;
                    if !counts.is_empty() {
                        counts[t] = counts[node as usize];
                    }
                    match reached {
                        true => queue.decrease(distance, target),
                        false => queue.push(distance, target),
                    }
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
                if w != v && distance[v] + weight == distance[w] {
                    gained += counts[v] / counts[w] * (1.0 + dependency[w]);
                }
                Ok(())
            })?;
            dependency[v] = gained;
        }
        Ok(())
    }
}

/// The position of a node not in the queue.
const NOT_QUEUED: usize = usize::MAX;

/// An indexed binary min-heap of nodes keyed by their tentative distance
/// (then dense id), with each node's position so a node is queued at most
/// once and its key can decrease in place.
struct Queue {
    heap: Vec<DenseId>,
    /// Each node's position in `heap`, or [`NOT_QUEUED`].
    at: Vec<usize>,
}

impl Queue {
    /// Whether `a` comes before `b`.
    fn before(distance: &[f64], a: DenseId, b: DenseId) -> bool {
        let (da, db) = (distance[a as usize], distance[b as usize]);
        da.total_cmp(&db).then(a.cmp(&b)).is_lt()
    }

    fn place(&mut self, position: usize, node: DenseId) {
        self.heap[position] = node;
        self.at[node as usize] = position;
    }

    /// Queues `node`, which is not queued; the heap has room for every node.
    fn push(&mut self, distance: &[f64], node: DenseId) {
        self.heap.push(node);
        self.at[node as usize] = self.heap.len() - 1;
        self.decrease(distance, node);
    }

    /// Restores the order after `node`'s distance fell.
    fn decrease(&mut self, distance: &[f64], node: DenseId) {
        let mut position = self.at[node as usize];
        while position > 0 {
            let parent = (position - 1) / 2;
            let above = self.heap[parent];
            if !Self::before(distance, node, above) {
                break;
            }
            self.place(position, above);
            position = parent;
        }
        self.place(position, node);
    }

    /// Takes the nearest node.
    fn pop(&mut self, distance: &[f64]) -> Option<DenseId> {
        let first = *self.heap.first()?;
        self.at[first as usize] = NOT_QUEUED;
        let Some(last) = self.heap.pop() else {
            return Some(first);
        };
        if self.heap.is_empty() {
            return Some(first);
        }
        let mut position = 0;
        loop {
            let left = 2 * position + 1;
            if left >= self.heap.len() {
                break;
            }
            let right = left + 1;
            let child = match right < self.heap.len()
                && Self::before(distance, self.heap[right], self.heap[left])
            {
                true => right,
                false => left,
            };
            if !Self::before(distance, self.heap[child], last) {
                break;
            }
            self.place(position, self.heap[child]);
            position = child;
        }
        self.place(position, last);
        Some(first)
    }
}
