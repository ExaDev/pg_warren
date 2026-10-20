//! The edge list's columns as a build consumes them, and their counting
//! sort into rows: a stable scatter by source, with no comparison sort and
//! no cursor array. Counts go to `offsets[source + 2]`, so after the prefix
//! sum `offsets[source + 1]` is where `source`'s row starts; scattering
//! advances it to where the row ends, which leaves `offsets` exactly the
//! CSR's offsets. A second column is scattered after shifting them back.

use std::ops::Range;

use super::super::Target;
use crate::graph::{BuildBuffer, BuildMemory, GraphError, RelationIndex};

/// The edge list's columns, with endpoints of width `S`.
pub(super) struct Columns<S> {
    pub(super) sources: BuildBuffer<S>,
    /// `None` when the dictionary has at most one relation, so every edge
    /// is relation 0.
    pub(super) relations: Option<BuildBuffer<RelationIndex>>,
    pub(super) targets: BuildBuffer<S>,
    pub(super) weights: Option<BuildBuffer<f64>>,
}

impl<S: Target> Columns<S> {
    /// The relation of `edge`.
    fn relation(&self, edge: usize) -> usize {
        self.relations.as_ref().map_or(0, |relations| relations[edge].index())
    }

    /// Frees every column.
    fn release(&mut self) {
        self.sources.release();
        self.targets.release();
        if let Some(relations) = &mut self.relations {
            relations.release();
        }
        if let Some(weights) = &mut self.weights {
            weights.release();
        }
    }
}

/// Edges grouped into rows by source: entries `offsets[v]..offsets[v + 1]`
/// of `targets` (and `weights`, `relations`) belong to source `v`, in
/// input order.
pub(super) struct Rows<T> {
    pub(super) offsets: BuildBuffer<u64>,
    pub(super) targets: BuildBuffer<T>,
    pub(super) weights: Option<BuildBuffer<f64>>,
    /// Each entry's relation, kept only for the merged CSR's weights.
    pub(super) relations: Option<BuildBuffer<RelationIndex>>,
    /// Whether a column has been scattered, so `offsets` are the CSR's
    /// rather than the scatter cursors.
    scattered: bool,
}

impl<T: Target> Rows<T> {
    /// Zeroed offsets for `id_space` rows and room for `edges` targets.
    fn new(memory: &BuildMemory, id_space: u64, edges: u64) -> Result<Self, GraphError> {
        Ok(Self {
            offsets: BuildBuffer::filled(memory, row_count(id_space)? + 1, 0)?,
            targets: BuildBuffer::filled(memory, length(edges)?, T::default())?,
            weights: None,
            relations: None,
            scattered: false,
        })
    }
}

/// The rows of the relations in `group`, one [`Rows`] each, scattered from
/// `columns`. With `release`, each input column is freed once scattered, and
/// the rest once all are: the edge list is then no longer needed.
pub(super) fn group_rows<T: Target, S: Target>(
    columns: &mut Columns<S>,
    group: &Range<usize>,
    relation_edges: &[u64],
    id_space: u64,
    release: bool,
) -> Result<Vec<Rows<T>>, GraphError> {
    let memory = columns.sources.memory().clone();
    let mut rows = group
        .clone()
        .map(|relation| Rows::new(&memory, id_space, relation_edges[relation]))
        .collect::<Result<Vec<_>, _>>()?;
    let slot = |columns: &Columns<S>, edge| {
        let relation = columns.relation(edge);
        group.contains(&relation).then(|| relation - group.start)
    };
    count(&columns.sources, |edge| slot(columns, edge), &mut rows);
    let allocate_weights = |rows: &mut [Rows<T>]| -> Result<(), GraphError> {
        for (relation, rows) in group.clone().zip(rows) {
            rows.weights =
                Some(BuildBuffer::filled(&memory, length(relation_edges[relation])?, 0.0)?);
        }
        Ok(())
    };
    match (&columns.weights, release) {
        // Inputs kept for later groups: targets and weights in one pass.
        (Some(weights), false) => {
            allocate_weights(&mut rows)?;
            scatter(
                &columns.sources,
                |edge| slot(columns, edge),
                &mut rows,
                |rows, at, edge| {
                    rows.targets[at] = target::<T, S>(columns.targets[edge]);
                    if let Some(out) = &mut rows.weights {
                        out[at] = weights[edge];
                    }
                },
            );
        }
        // Inputs freed as they go: targets first, then weights.
        (weights, _) => {
            scatter(
                &columns.sources,
                |edge| slot(columns, edge),
                &mut rows,
                |rows, at, edge| {
                    rows.targets[at] = target::<T, S>(columns.targets[edge]);
                },
            );
            if release {
                columns.targets.release();
            }
            if let Some(weights) = weights {
                allocate_weights(&mut rows)?;
                scatter(
                    &columns.sources,
                    |edge| slot(columns, edge),
                    &mut rows,
                    |rows, at, edge| {
                        if let Some(out) = &mut rows.weights {
                            out[at] = weights[edge];
                        }
                    },
                );
            }
        }
    }
    if release {
        columns.release();
    }
    Ok(rows)
}

/// The rows of every edge, for the merged CSR, scattered from `columns`,
/// which are freed one by one as they are scattered. Each entry keeps its
/// relation when weighted, to sum weights per relation first.
pub(super) fn merged_rows<T: Target, S: Target>(
    mut columns: Columns<S>,
    id_space: u64,
) -> Result<Rows<T>, GraphError> {
    let memory = columns.sources.memory().clone();
    let edges = columns.sources.len();
    let mut rows = [Rows::new(&memory, id_space, edges as u64)?];
    let all = |_| Some(0);
    count(&columns.sources, all, &mut rows);
    scatter(&columns.sources, all, &mut rows, |rows, at, edge| {
        rows.targets[at] = target::<T, S>(columns.targets[edge]);
    });
    columns.targets.release();
    if let Some(input) = &columns.weights {
        if let Some(relations) = &columns.relations {
            rows[0].relations =
                Some(BuildBuffer::filled(&memory, edges, RelationIndex::default())?);
            scatter(&columns.sources, all, &mut rows, |rows, at, edge| {
                if let Some(out) = &mut rows.relations {
                    out[at] = relations[edge];
                }
            });
        }
        if let Some(relations) = &mut columns.relations {
            relations.release();
        }
        rows[0].weights = Some(BuildBuffer::filled(&memory, edges, 0.0)?);
        scatter(&columns.sources, all, &mut rows, |rows, at, edge| {
            if let Some(out) = &mut rows.weights {
                out[at] = input[edge];
            }
        });
    }
    columns.release();
    let [rows] = rows;
    Ok(rows)
}

/// Counts each selected edge into `offsets[source + 2]` of its slot's
/// rows, then prefix-sums, so `offsets[source + 1]` is the row's start.
fn count<S: Target, T>(sources: &[S], slot: impl Fn(usize) -> Option<usize>, rows: &mut [Rows<T>]) {
    for (edge, source) in sources.iter().enumerate() {
        if let Some(slot) = slot(edge) {
            if let Some(count) = rows[slot].offsets.get_mut(source.dense() as usize + 2) {
                *count += 1;
            }
        }
    }
    for rows in rows {
        for row in 1..rows.offsets.len() {
            rows.offsets[row] += rows.offsets[row - 1];
        }
    }
}

/// Scatters selected edges into row order, stable in input order: `place`
/// fills the row entry at the given position from the given edge. Leaves
/// each slot's offsets the CSR's; offsets left by an earlier scatter are
/// first shifted back to the row starts.
fn scatter<S: Target, T>(
    sources: &[S],
    slot: impl Fn(usize) -> Option<usize>,
    rows: &mut [Rows<T>],
    mut place: impl FnMut(&mut Rows<T>, usize, usize),
) {
    for rows in rows.iter_mut() {
        if rows.scattered {
            shift(&mut rows.offsets);
        }
        rows.scattered = true;
    }
    for (edge, source) in sources.iter().enumerate() {
        let Some(slot) = slot(edge) else { continue };
        let rows = &mut rows[slot];
        let cursor = &mut rows.offsets[source.dense() as usize + 1];
        let position = *cursor as usize;
        *cursor += 1;
        place(rows, position, edge);
    }
}

/// Turns CSR offsets (`offsets[v]`, the start of row `v`) back into scatter
/// cursors (`offsets[v + 1]`, the start of row `v`).
fn shift(offsets: &mut [u64]) {
    let rows = offsets.len() - 1;
    offsets.copy_within(0..rows, 1);
    offsets[0] = 0;
}

/// A source column's target at the CSR's target width; the plan checked
/// that every endpoint lies in the id space, which the width covers.
fn target<T: Target, S: Target>(target: S) -> T {
    T::from_dense(target.dense()).unwrap_or_default()
}

/// The number of rows of an id space, as an index.
fn row_count(id_space: u64) -> Result<usize, GraphError> {
    usize::try_from(id_space).map_err(|_| GraphError::IdSpaceTooLarge)
}

/// An edge count as a length.
fn length(edges: u64) -> Result<usize, GraphError> {
    usize::try_from(edges).map_err(|_| GraphError::OutOfMemory { elements: usize::MAX })
}
