//! Forward CSR construction without a comparison sort of the edge list
//! (DESIGN.md §5.4 step 4): counting sort by source, then sort and
//! deduplicate each row by (relation, target), then split per relation.
//!
//! Memory: the edge list is scattered into rows one column at a time, and
//! each input column is freed as soon as it has been scattered, so at most
//! one column exists twice. The rows are freed before the merged CSR is
//! built from the per-relation CSRs.

use std::ops::Range;

use super::{Csr, Target};
use crate::graph::edge_list::{EdgeColumns, Endpoints};
use crate::graph::{memory, EdgeList, GraphError, RelationIndex};

/// The forward CSRs of one build.
pub(super) struct Forward<T> {
    /// One CSR per relation, in relation-index order.
    pub(super) relations: Vec<Csr<T>>,
    /// The all-relations CSR, if stored.
    pub(super) merged: Option<Csr<T>>,
    /// Deduplicated edges: parallel edges folded into another edge of the
    /// same relation.
    pub(super) deduplicated_edges: u64,
}

/// Builds the forward CSRs of `edges`, consuming them, and the merged CSR
/// if `store_merged`.
pub(super) fn forward<T: Target>(
    edges: EdgeList,
    relation_count: usize,
    id_space: u64,
    store_merged: bool,
) -> Result<Forward<T>, GraphError> {
    let EdgeColumns { relations, endpoints, weights } = edges.into_columns();
    let rows = match endpoints {
        Endpoints::U32 { sources, targets } => {
            let columns = Columns { relations, sources, targets, weights };
            grouped_by_source::<T, u32>(columns, relation_count, id_space)
        }
        Endpoints::U64 { sources, targets } => {
            let columns = Columns { relations, sources, targets, weights };
            grouped_by_source::<T, u64>(columns, relation_count, id_space)
        }
    }?;
    let (rows, deduplicated_edges) = deduplicated(rows)?;
    let relations = split_by_relation(rows, relation_count, id_space)?;
    let merged = match store_merged {
        true => Some(united(&relations, id_space)?),
        false => None,
    };
    Ok(Forward { relations, merged, deduplicated_edges })
}

/// The edge list's columns, with endpoints of width `S`.
struct Columns<S> {
    relations: Vec<RelationIndex>,
    sources: Vec<S>,
    targets: Vec<S>,
    weights: Option<Vec<f64>>,
}

/// Edges grouped into rows by source: entries `offsets[v]..offsets[v + 1]`
/// of `relations`, `targets` and `weights` belong to source `v`.
struct Rows<T> {
    offsets: Vec<u64>,
    relations: Vec<u32>,
    targets: Vec<T>,
    weights: Option<Vec<f64>>,
}

impl<T> Rows<T> {
    fn row_count(&self) -> usize {
        self.offsets.len() - 1
    }

    fn row(&self, source: usize) -> Range<usize> {
        self.offsets[source] as usize..self.offsets[source + 1] as usize
    }

    fn weight(&self, entry: usize) -> f64 {
        self.weights.as_ref().map_or(0.0, |weights| weights[entry])
    }
}

/// Counting sort of the edge list by source, stable in input order. Each
/// input column is dropped once scattered.
fn grouped_by_source<T: Target, S: Target>(
    columns: Columns<S>,
    relation_count: usize,
    id_space: u64,
) -> Result<Rows<T>, GraphError> {
    let offsets = row_offsets::<T, S>(&columns, relation_count, id_space)?;
    let Columns { relations: input_relations, sources, targets: input_targets, weights } = columns;
    let targets = scatter(&sources, &offsets, |edge| {
        T::from_dense(input_targets[edge].dense()).unwrap_or_default()
    })?;
    drop(input_targets);
    let relations = scatter(&sources, &offsets, |edge| input_relations[edge].index() as u32)?;
    drop(input_relations);
    let weights = match weights {
        Some(input_weights) => Some(scatter(&sources, &offsets, |edge| input_weights[edge])?),
        None => None,
    };
    Ok(Rows { offsets, relations, targets, weights })
}

/// Validates every edge and counts them per source into row offsets.
fn row_offsets<T: Target, S: Target>(
    columns: &Columns<S>,
    relation_count: usize,
    id_space: u64,
) -> Result<Vec<u64>, GraphError> {
    if id_space == u64::MAX {
        return Err(GraphError::IdSpaceTooLarge);
    }
    let row_count = id_space as usize;
    let mut offsets = memory::filled(row_count + 1, 0u64)?;
    let edges = columns.sources.iter().zip(&columns.targets).zip(&columns.relations);
    for ((source, target), relation) in edges {
        let (source, target) = (source.dense(), target.dense());
        if source >= id_space || target >= id_space || T::from_dense(target).is_none() {
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
    Ok(offsets)
}

/// One column scattered into row order: each edge lands at the next free
/// slot of its source's row, so rows keep input order.
fn scatter<S: Target, V: Copy + Default>(
    sources: &[S],
    offsets: &[u64],
    value: impl Fn(usize) -> V,
) -> Result<Vec<V>, GraphError> {
    let row_count = offsets.len() - 1;
    let mut cursor = memory::with_capacity(row_count)?;
    cursor.extend_from_slice(&offsets[..row_count]);
    let mut column = memory::filled(sources.len(), V::default())?;
    for (edge, source) in sources.iter().enumerate() {
        let source = source.dense() as usize;
        column[cursor[source] as usize] = value(edge);
        cursor[source] += 1;
    }
    Ok(column)
}

/// Sorts each row by (relation, target) and folds parallel edges into one,
/// summing their weights in input order. Returns the folded count.
fn deduplicated<T: Target>(mut rows: Rows<T>) -> Result<(Rows<T>, u64), GraphError> {
    let mut scratch: Vec<(u32, T, f64)> = Vec::new();
    let mut deduplicated_edges = 0;
    let mut write: usize = 0;
    let mut start = 0;
    for source in 0..rows.row_count() {
        let end = rows.offsets[source + 1] as usize;
        scratch.clear();
        reserve(&mut scratch, end - start)?;
        scratch.extend((start..end).map(|i| (rows.relations[i], rows.targets[i], rows.weight(i))));
        scratch.sort_by_key(|&(relation, target, _)| (relation, target));
        let row_start = write;
        for &(relation, target, weight) in &scratch {
            let last = write.checked_sub(1).filter(|&last| last >= row_start);
            match last {
                Some(last) if (rows.relations[last], rows.targets[last]) == (relation, target) => {
                    if let Some(weights) = &mut rows.weights {
                        weights[last] += weight;
                    }
                    deduplicated_edges += 1;
                }
                _ => {
                    rows.relations[write] = relation;
                    rows.targets[write] = target;
                    if let Some(weights) = &mut rows.weights {
                        weights[write] = weight;
                    }
                    write += 1;
                }
            }
        }
        rows.offsets[source + 1] = write as u64;
        start = end;
    }
    rows.relations.truncate(write);
    rows.targets.truncate(write);
    if let Some(weights) = &mut rows.weights {
        weights.truncate(write);
    }
    Ok((rows, deduplicated_edges))
}

/// One CSR per relation from deduplicated rows, which are consumed.
fn split_by_relation<T: Target>(
    rows: Rows<T>,
    relation_count: usize,
    id_space: u64,
) -> Result<Vec<Csr<T>>, GraphError> {
    let mut sizes = vec![0; relation_count];
    for &relation in &rows.relations {
        sizes[relation as usize] += 1;
    }
    let weighted = rows.weights.is_some();
    let mut builders = sizes
        .into_iter()
        .map(|size| CsrBuilder::new(rows.row_count(), size, weighted))
        .collect::<Result<Vec<_>, _>>()?;
    for source in 0..rows.row_count() {
        for entry in rows.row(source) {
            builders[rows.relations[entry] as usize].push(rows.targets[entry], rows.weight(entry));
        }
        builders.iter_mut().for_each(CsrBuilder::end_row);
    }
    drop(rows);
    builders.into_iter().map(|builder| builder.finish(id_space)).collect()
}

/// The all-relations CSR: each row's targets across relations, with the
/// weights of a target that occurs in several relations summed in
/// relation order.
fn united<T: Target>(relations: &[Csr<T>], id_space: u64) -> Result<Csr<T>, GraphError> {
    let edge_count = relations.iter().map(|csr| csr.targets().len()).sum();
    let weighted = relations.first().is_some_and(Csr::is_weighted);
    let mut builder = CsrBuilder::new(id_space as usize, edge_count, weighted)?;
    let mut scratch: Vec<(T, f64)> = Vec::new();
    for source in 0..id_space {
        scratch.clear();
        for csr in relations {
            let targets = csr.neighbours(source);
            let weights = csr.neighbour_weights(source);
            reserve(&mut scratch, targets.len())?;
            scratch.extend(
                targets.iter().enumerate().map(|(i, &t)| (t, weights.map_or(0.0, |w| w[i]))),
            );
        }
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
