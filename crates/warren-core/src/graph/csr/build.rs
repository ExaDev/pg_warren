//! Forward CSR construction without a comparison sort of the edge list
//! (DESIGN.md §5.4 step 4): counting sort by source, then sort and
//! deduplicate each row by (relation, target), then split per relation.

use super::{Csr, Target};
use crate::graph::{memory, EdgeList, GraphError};

/// The forward CSRs of one build.
pub(super) struct Forward<T> {
    /// One CSR per relation, in relation-index order.
    pub(super) relations: Vec<Csr<T>>,
    /// The all-relations CSR, unless there is exactly one relation.
    pub(super) merged: Option<Csr<T>>,
    /// Deduplicated edges: parallel edges folded into another edge of the
    /// same relation.
    pub(super) deduplicated_edges: u64,
}

/// Builds the forward CSRs of `edges`, consuming them.
pub(super) fn forward<T: Target>(
    edges: EdgeList,
    relation_count: usize,
    id_space: u64,
) -> Result<Forward<T>, GraphError> {
    let rows = grouped_by_source::<T>(edges, relation_count, id_space)?;
    let (rows, deduplicated_edges) = deduplicated(rows)?;
    let relations = split_by_relation(&rows, relation_count, id_space)?;
    let merged = match relation_count {
        1 => None,
        _ => Some(united(&rows, id_space)?),
    };
    Ok(Forward { relations, merged, deduplicated_edges })
}

/// Edges grouped into rows by source: `entries[offsets[v]..offsets[v + 1]]`
/// are the (relation, target) pairs of source `v`.
struct Rows<T> {
    offsets: Vec<u64>,
    entries: Vec<(u32, T)>,
    weights: Option<Vec<f64>>,
}

impl<T> Rows<T> {
    fn row_count(&self) -> usize {
        self.offsets.len() - 1
    }

    fn row(&self, source: usize) -> std::ops::Range<usize> {
        self.offsets[source] as usize..self.offsets[source + 1] as usize
    }

    fn weight(&self, entry: usize) -> f64 {
        self.weights.as_ref().map_or(0.0, |weights| weights[entry])
    }
}

/// Counting sort of the edge list by source, stable in input order.
fn grouped_by_source<T: Target>(
    edges: EdgeList,
    relation_count: usize,
    id_space: u64,
) -> Result<Rows<T>, GraphError> {
    if id_space == u64::MAX {
        return Err(GraphError::IdSpaceTooLarge);
    }
    let row_count = id_space as usize;
    let mut offsets = memory::filled(row_count + 1, 0u64)?;
    for ((&source, &target), relation) in
        edges.sources().iter().zip(edges.targets()).zip(edges.relations())
    {
        if source >= id_space || target >= id_space {
            return Err(GraphError::EdgeOutsideIdSpace { from: source, to: target });
        }
        if relation.index() >= relation_count {
            return Err(GraphError::RelationOutsideDictionary {
                relation: relation.index() as u64,
            });
        }
        offsets[source as usize + 1] += 1;
    }
    for row in 1..=row_count {
        offsets[row] += offsets[row - 1];
    }
    let mut cursor = memory::with_capacity(row_count)?;
    cursor.extend_from_slice(&offsets[..row_count]);
    let mut entries = memory::filled(edges.len(), (0u32, T::default()))?;
    let mut weights = match edges.weights() {
        Some(_) => Some(memory::filled(edges.len(), 0.0)?),
        None => None,
    };
    for edge in 0..edges.len() {
        let source = edges.sources()[edge] as usize;
        let target = edges.targets()[edge];
        let target = T::from_dense(target)
            .ok_or(GraphError::EdgeOutsideIdSpace { from: source as u64, to: target })?;
        let position = cursor[source] as usize;
        cursor[source] += 1;
        entries[position] = (edges.relations()[edge].index() as u32, target);
        if let (Some(sorted), Some(input)) = (&mut weights, edges.weights()) {
            sorted[position] = input[edge];
        }
    }
    Ok(Rows { offsets, entries, weights })
}

/// Sorts each row by (relation, target) and folds parallel edges into one,
/// summing their weights in input order. Returns the folded count.
fn deduplicated<T: Target>(mut rows: Rows<T>) -> Result<(Rows<T>, u64), GraphError> {
    let mut scratch: Vec<(u32, T, f64)> = Vec::new();
    let mut deduplicated_edges = 0;
    let mut write = 0;
    let mut start = 0;
    for source in 0..rows.row_count() {
        let end = rows.offsets[source + 1] as usize;
        scratch.clear();
        reserve(&mut scratch, end - start)?;
        scratch
            .extend((start..end).map(|i| (rows.entries[i].0, rows.entries[i].1, rows.weight(i))));
        scratch.sort_by_key(|&(relation, target, _)| (relation, target));
        let row_start = write;
        for &(relation, target, weight) in &scratch {
            if write > row_start && rows.entries[write - 1] == (relation, target) {
                if let Some(weights) = &mut rows.weights {
                    weights[write - 1] += weight;
                }
                deduplicated_edges += 1;
            } else {
                rows.entries[write] = (relation, target);
                if let Some(weights) = &mut rows.weights {
                    weights[write] = weight;
                }
                write += 1;
            }
        }
        rows.offsets[source + 1] = write as u64;
        start = end;
    }
    rows.entries.truncate(write);
    if let Some(weights) = &mut rows.weights {
        weights.truncate(write);
    }
    Ok((rows, deduplicated_edges))
}

/// One CSR per relation from deduplicated rows.
fn split_by_relation<T: Target>(
    rows: &Rows<T>,
    relation_count: usize,
    id_space: u64,
) -> Result<Vec<Csr<T>>, GraphError> {
    let mut sizes = vec![0; relation_count];
    for &(relation, _) in &rows.entries {
        sizes[relation as usize] += 1;
    }
    let weighted = rows.weights.is_some();
    let mut builders = sizes
        .into_iter()
        .map(|size| CsrBuilder::new(rows.row_count(), size, weighted))
        .collect::<Result<Vec<_>, _>>()?;
    for source in 0..rows.row_count() {
        for entry in rows.row(source) {
            let (relation, target) = rows.entries[entry];
            builders[relation as usize].push(target, rows.weight(entry));
        }
        builders.iter_mut().for_each(CsrBuilder::end_row);
    }
    builders.into_iter().map(|builder| builder.finish(id_space)).collect()
}

/// The all-relations CSR: each row's targets across relations, with the
/// weights of a target that occurs in several relations summed.
fn united<T: Target>(rows: &Rows<T>, id_space: u64) -> Result<Csr<T>, GraphError> {
    let mut builder =
        CsrBuilder::new(rows.row_count(), rows.entries.len(), rows.weights.is_some())?;
    let mut scratch: Vec<(T, f64)> = Vec::new();
    for source in 0..rows.row_count() {
        let row = rows.row(source);
        scratch.clear();
        reserve(&mut scratch, row.len())?;
        scratch.extend(row.map(|i| (rows.entries[i].1, rows.weight(i))));
        scratch.sort_by_key(|&(target, _)| target);
        let mut previous = None;
        for &(target, weight) in &scratch {
            if previous == Some(target) {
                builder.add_to_last_weight(weight);
            } else {
                builder.push(target, weight);
                previous = Some(target);
            }
        }
        builder.end_row();
    }
    builder.finish(id_space)
}

fn reserve<E>(scratch: &mut Vec<E>, additional: usize) -> Result<(), GraphError> {
    scratch.try_reserve(additional).map_err(|_| GraphError::OutOfMemory { elements: additional })
}

/// Appends rows to a CSR whose sizes are known up front.
struct CsrBuilder<T> {
    offsets: Vec<u64>,
    targets: Vec<T>,
    weights: Option<Vec<f64>>,
}

impl<T: Target> CsrBuilder<T> {
    fn new(row_count: usize, edge_count: usize, weighted: bool) -> Result<Self, GraphError> {
        let mut offsets = memory::with_capacity(row_count + 1)?;
        offsets.push(0);
        Ok(Self {
            offsets,
            targets: memory::with_capacity(edge_count)?,
            weights: match weighted {
                true => Some(memory::with_capacity(edge_count)?),
                false => None,
            },
        })
    }

    fn push(&mut self, target: T, weight: f64) {
        self.targets.push(target);
        if let Some(weights) = &mut self.weights {
            weights.push(weight);
        }
    }

    fn add_to_last_weight(&mut self, weight: f64) {
        if let Some(last) = self.weights.as_mut().and_then(|weights| weights.last_mut()) {
            *last += weight;
        }
    }

    fn end_row(&mut self) {
        self.offsets.push(self.targets.len() as u64);
    }

    fn finish(self, id_space: u64) -> Result<Csr<T>, GraphError> {
        Csr::from_parts(id_space, self.offsets, self.targets, self.weights)
    }
}
