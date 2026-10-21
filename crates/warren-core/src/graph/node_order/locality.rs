//! The locality order: Reverse Cuthill–McKee over the undirected view of an
//! edge list (DESIGN.md §14 item 22).

use super::Permutation;
use crate::graph::edge_list::Endpoints;
use crate::graph::{BuildBuffer, BuildMemory, DenseId, EdgeList, GraphError, Target, TargetWidth};

/// Marks a node not yet numbered.
const UNPLACED: DenseId = DenseId::MAX;

/// [`Permutation::locality`].
pub(super) fn order(
    edges: &EdgeList,
    id_space: u64,
    memory: &BuildMemory,
) -> Result<Permutation, GraphError> {
    match edges.columns().1 {
        Endpoints::U32 { sources, targets } => order_as(sources, targets, id_space, memory),
        Endpoints::U64 { sources, targets } => order_as(sources, targets, id_space, memory),
    }
}

/// [`order`] over endpoint columns of width `T`, which the scratch shares.
fn order_as<T: Target>(
    sources: &[T],
    targets: &[T],
    id_space: u64,
    memory: &BuildMemory,
) -> Result<Permutation, GraphError> {
    if T::TARGET_WIDTH == TargetWidth::U32
        && TargetWidth::for_id_space(id_space) == TargetWidth::U64
    {
        return Err(GraphError::IdSpaceExceedsTargetWidth);
    }
    let nodes = usize::try_from(id_space).map_err(|_| GraphError::IdSpaceTooLarge)?;
    let adjacency = Undirected::new(sources, targets, nodes, memory)?;
    let starts = adjacency.by_degree(memory)?;
    let mut new_by_old = BuildBuffer::filled(memory, nodes, UNPLACED)?;
    let mut order = BuildBuffer::<T>::with_capacity(memory, nodes)?;
    for &start in starts.iter() {
        if new_by_old[start.dense() as usize] == UNPLACED {
            adjacency.search(start, &mut order, &mut new_by_old)?;
        }
    }
    drop((starts, adjacency));
    for (position, &old) in order.iter().rev().enumerate() {
        new_by_old[old.dense() as usize] = position as DenseId;
    }
    Ok(Permutation { new_by_old })
}

/// The undirected view of an edge list, without self-loops or parallel
/// edges, as a CSR: each row sorted and distinct.
struct Undirected<T> {
    offsets: BuildBuffer<u64>,
    neighbours: BuildBuffer<T>,
}

impl<T: Target> Undirected<T> {
    /// Counts each edge at both endpoints, scatters it into both rows, then
    /// sorts each row and folds its repeats in place.
    fn new(
        sources: &[T],
        targets: &[T],
        nodes: usize,
        memory: &BuildMemory,
    ) -> Result<Self, GraphError> {
        let rows = nodes.checked_add(2).ok_or(GraphError::IdSpaceTooLarge)?;
        // Counts go to offsets[node + 2], so that after the prefix sum
        // offsets[node + 1] is the row's start and serves as its cursor.
        let mut offsets = BuildBuffer::filled(memory, rows, 0u64)?;
        for (&source, &target) in sources.iter().zip(targets) {
            let (source, target) = (source.dense(), target.dense());
            if source.max(target) >= nodes as u64 {
                return Err(GraphError::EdgeOutsideIdSpace { from: source, to: target });
            }
            if source != target {
                offsets[source as usize + 2] += 1;
                offsets[target as usize + 2] += 1;
            }
        }
        for row in 2..rows {
            offsets[row] += offsets[row - 1];
        }
        let entries = usize::try_from(offsets[rows - 1])
            .map_err(|_| GraphError::OutOfMemory { elements: usize::MAX })?;
        let mut neighbours = BuildBuffer::filled(memory, entries, T::default())?;
        for (&source, &target) in sources.iter().zip(targets) {
            if source != target {
                for (from, to) in [(source, target), (target, source)] {
                    let cursor = &mut offsets[from.dense() as usize + 1];
                    neighbours[*cursor as usize] = to;
                    *cursor += 1;
                }
            }
        }
        let mut adjacency = Self { offsets, neighbours };
        adjacency.fold_rows(nodes);
        Ok(adjacency)
    }

    /// Sorts every row and removes its repeats, moving the rows down to
    /// close the gaps.
    fn fold_rows(&mut self, nodes: usize) {
        let (offsets, neighbours) = (&mut self.offsets, &mut self.neighbours);
        let (mut start, mut write) = (0, 0);
        for node in 0..nodes {
            let end = offsets[node + 1] as usize;
            neighbours[start..end].sort_unstable();
            offsets[node] = write as u64;
            for read in start..end {
                if read == start || neighbours[read] != neighbours[read - 1] {
                    neighbours[write] = neighbours[read];
                    write += 1;
                }
            }
            start = end;
        }
        offsets[nodes] = write as u64;
        neighbours.truncate(write);
    }

    fn row(&self, node: T) -> &[T] {
        let node = node.dense() as usize;
        &self.neighbours[self.offsets[node] as usize..self.offsets[node + 1] as usize]
    }

    fn degree(&self, node: T) -> u64 {
        let node = node.dense() as usize;
        self.offsets[node + 1] - self.offsets[node]
    }

    /// Every node by ascending degree, then id: the order in which
    /// components are started. A counting sort.
    fn by_degree(&self, memory: &BuildMemory) -> Result<BuildBuffer<T>, GraphError> {
        let nodes = self.offsets.len() - 2;
        let ids = (0..nodes as u64).filter_map(T::from_dense);
        let most = ids.clone().map(|node| self.degree(node)).max().unwrap_or(0) as usize;
        let mut starts = BuildBuffer::filled(memory, most + 2, 0u64)?;
        for node in ids.clone() {
            starts[self.degree(node) as usize + 1] += 1;
        }
        for degree in 1..starts.len() {
            starts[degree] += starts[degree - 1];
        }
        let mut sorted = BuildBuffer::filled(memory, nodes, T::default())?;
        for node in ids {
            let cursor = &mut starts[self.degree(node) as usize];
            sorted[*cursor as usize] = node;
            *cursor += 1;
        }
        Ok(sorted)
    }

    /// Cuthill–McKee from `start`: appends its component to `order`
    /// breadth first, each node's unplaced neighbours by ascending degree,
    /// then id, marking each in `placed` as it is appended.
    fn search(
        &self,
        start: T,
        order: &mut BuildBuffer<T>,
        placed: &mut [DenseId],
    ) -> Result<(), GraphError> {
        let mut head = order.len();
        placed[start.dense() as usize] = 0;
        order.push(start)?;
        while head < order.len() {
            let node = order[head];
            head += 1;
            let first = order.len();
            for &neighbour in self.row(node) {
                if placed[neighbour.dense() as usize] == UNPLACED {
                    placed[neighbour.dense() as usize] = 0;
                    order.push(neighbour)?;
                }
            }
            order[first..].sort_unstable_by_key(|&neighbour| (self.degree(neighbour), neighbour));
        }
        Ok(())
    }
}
