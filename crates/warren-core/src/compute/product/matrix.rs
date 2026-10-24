//! The adjacency matrix a product reads: the CSRs of the selected relations
//! in the orientation the product needs, an edge in several of them one
//! entry, its weights summed in relation order (1 when unweighted); or,
//! per relation, one entry in each.

use std::mem::size_of;

use super::super::{ComputeError, ComputeMemory, MemoryCharge};
use crate::graph::RelationSelection;
use crate::graph::{AnyCsr, AnyCsrSlices, CsrSlices, DenseId, GraphCsr, Target};

/// How a kernel reads the selected relations' edges: with their weights or
/// not, and a pair linked in several relations as one edge or as an edge
/// in each (DESIGN.md §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    /// Use edge weights (`None`: if the graph has them).
    pub weighted: Option<bool>,
    /// Read a pair in several relations as an edge in each.
    pub per_relation: bool,
}

/// How a product walks the matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Form {
    /// Row `i` of the CSRs read is row `i` of the result.
    Pull,
    /// Row `i` of the CSRs read scatters into the rows of its targets.
    Push,
}

/// The CSRs a product reads, their arrays borrowed once rather than on
/// every row.
#[derive(Debug)]
pub(crate) struct Matrix<'g> {
    csrs: Vec<AnyCsrSlices<'g>>,
    weighted: bool,
    /// An edge in several relations is an entry in each, not one entry.
    per_relation: bool,
    form: Form,
    /// The budget row scratches are charged to.
    memory: ComputeMemory,
}

/// What a reader of rows reuses across rows: the entries of one row of
/// several CSRs, and where each CSR's last row was found, so rows read in
/// ascending order find sparse offsets' rows without a binary search each
/// (DESIGN.md §5.3).
#[derive(Debug)]
pub(crate) struct RowScratch {
    entries: RowEntries,
    hints: RowHints,
}

/// The entries of one row of several CSRs, sorted and folded: a buffer
/// reused across rows, its capacity charged to the compute budget before
/// it is reserved.
#[derive(Debug)]
struct RowEntries {
    entries: Vec<(DenseId, f64)>,
    charge: MemoryCharge,
}

/// Where each CSR's last row was found: one CSR's hint inline, or, for
/// several CSRs, a buffer of a hint per CSR, its capacity charged to the
/// compute budget before it is reserved.
#[derive(Debug)]
struct RowHints {
    one: usize,
    hints: Vec<usize>,
    charge: MemoryCharge,
}

impl RowScratch {
    /// An empty scratch, its capacity charged to `memory` as it grows: on
    /// the pool, beyond any bound reclaimed for ahead (DESIGN.md §5.5).
    pub(crate) fn new(memory: &ComputeMemory) -> Result<Self, ComputeError> {
        let entries = RowEntries { entries: Vec::new(), charge: memory.charge(0)?.unbounded() };
        let charge = memory.charge(0)?.unbounded();
        let hints = RowHints { one: 0, hints: Vec::new(), charge };
        Ok(Self { entries, hints })
    }
}

impl RowHints {
    /// A hint for each of `csrs` CSRs: those kept from earlier rows, and 0
    /// for each CSR more than the buffer held. One CSR's needs no buffer.
    fn for_csrs(&mut self, csrs: usize) -> Result<&mut [usize], ComputeError> {
        if csrs == 1 {
            return Ok(std::slice::from_mut(&mut self.one));
        }
        if let Some(more) = csrs.checked_sub(self.hints.len()).filter(|&more| more > 0) {
            let bytes = (more * size_of::<usize>()) as u64;
            self.charge.grow(bytes)?;
            self.hints.try_reserve_exact(more).map_err(|_| {
                self.charge.shrink(bytes);
                ComputeError::OutOfMemory { bytes }
            })?;
            self.hints.resize(csrs, 0);
        }
        Ok(&mut self.hints[..csrs])
    }
}

impl RowEntries {
    /// Empties the buffer and makes room for `len` entries.
    fn clear_for(&mut self, len: usize) -> Result<&mut Vec<(DenseId, f64)>, ComputeError> {
        self.entries.clear();
        let capacity = self.entries.capacity();
        if len > capacity {
            let bytes = ((len - capacity) * size_of::<(DenseId, f64)>()) as u64;
            self.charge.grow(bytes)?;
            self.entries.try_reserve_exact(len).map_err(|_| {
                self.charge.shrink(bytes);
                ComputeError::OutOfMemory { bytes }
            })?;
        }
        Ok(&mut self.entries)
    }
}

impl<'g> Matrix<'g> {
    /// The CSRs of `relations` for `y = A ⊗ x`, or `y = Aᵀ ⊗ x` if
    /// `transpose`: the forward CSRs pulled, or for `Aᵀ` the reverse CSRs
    /// pulled, else the forward CSRs pushed. An edge in several relations
    /// is one entry, its weights summed (1 when unweighted), or with
    /// `per_relation` an entry in each, read from every relation's CSRs
    /// rather than the stored merged CSR.
    pub(crate) fn new(
        graph: &'g GraphCsr,
        relations: RelationSelection<'_>,
        transpose: bool,
        weighted: Option<bool>,
        per_relation: bool,
        memory: &ComputeMemory,
    ) -> Result<Self, ComputeError> {
        let mut selected = graph
            .selected(relations)
            .map_err(|relation| ComputeError::UnknownRelation { relation: relation.index() })?;
        let has_weights = selected.iter().any(|r| r.forward().is_weighted());
        let weighted = weighted.unwrap_or(true) && has_weights;
        if per_relation && relations == RelationSelection::All {
            selected = graph.relations().iter().collect();
        }
        let reverse: Option<Vec<&AnyCsr>> = selected.iter().map(|r| r.reverse()).collect();
        let (csrs, form) = match (transpose, reverse) {
            (true, Some(reverse)) => (reverse, Form::Pull),
            (true, None) => (selected.iter().map(|r| r.forward()).collect(), Form::Push),
            (false, _) => (selected.iter().map(|r| r.forward()).collect(), Form::Pull),
        };
        Ok(Self {
            csrs: csrs.into_iter().map(AnyCsr::slices).collect(),
            weighted,
            per_relation,
            form,
            memory: memory.clone(),
        })
    }

    /// A row scratch for [`Self::for_each_entry`], charged to the budget.
    pub(crate) fn scratch(&self) -> Result<RowScratch, ComputeError> {
        RowScratch::new(&self.memory)
    }

    /// Whether entries carry the edges' weights (else every weight is 1).
    pub(crate) fn is_weighted(&self) -> bool {
        self.weighted
    }

    /// Whether an edge in several relations is an entry in each.
    pub(crate) fn is_per_relation(&self) -> bool {
        self.per_relation
    }

    /// How the product walks the CSRs.
    pub(super) fn form(&self) -> Form {
        self.form
    }

    /// Calls `entry(column, weight)` for each entry of `row` in ascending
    /// column order: an edge in several selected relations is one entry
    /// whose weight is their sum (an entry in each, in relation order, if
    /// per relation), and every weight is 1 when unweighted. `scratch` is
    /// reused across rows.
    pub(crate) fn for_each_entry(
        &self,
        row: DenseId,
        scratch: &mut RowScratch,
        mut entry: impl FnMut(DenseId, f64) -> Result<(), ComputeError>,
    ) -> Result<(), ComputeError> {
        // A hint per CSR, kept across rows; a scratch read with another
        // matrix starts them again.
        let RowScratch { entries: scratch, hints } = scratch;
        match (self.csrs.as_slice(), hints.for_csrs(self.csrs.len())?) {
            ([AnyCsrSlices::U32(csr)], [hint]) => entries(csr, row, hint, self.weighted, entry),
            ([AnyCsrSlices::U64(csr)], [hint]) => entries(csr, row, hint, self.weighted, entry),
            (many, hints) => {
                // Each row is sought twice, the second time at its hint.
                let len = many
                    .iter()
                    .zip(hints.iter_mut())
                    .map(|(csr, hint)| csr.seek(row, hint).len())
                    .sum();
                let scratch = scratch.clear_for(len)?;
                for (csr, hint) in many.iter().zip(hints.iter_mut()) {
                    let positions = csr.seek(row, hint);
                    let weights = csr.weights_at(positions.clone()).filter(|_| self.weighted);
                    scratch.extend(csr.neighbours_at(positions).enumerate().map(|(i, column)| {
                        (column, weights.and_then(|w| w.get(i)).copied().unwrap_or(1.0))
                    }));
                }
                scratch.sort_by_key(|&(column, _)| column);
                if self.per_relation {
                    return scratch.iter().try_for_each(|&(column, weight)| entry(column, weight));
                }
                for run in scratch.chunk_by(|a, b| a.0 == b.0) {
                    let weight = match self.weighted {
                        true => run.iter().map(|&(_, weight)| weight).sum(),
                        false => 1.0,
                    };
                    entry(run[0].0, weight)?;
                }
                Ok(())
            }
        }
    }
}

/// The CSRs of one relation's row, weighted or with unit weights, sought
/// from `hint`.
fn entries<T: Target>(
    csr: &CsrSlices<'_, T>,
    row: DenseId,
    hint: &mut usize,
    weighted: bool,
    mut entry: impl FnMut(DenseId, f64) -> Result<(), ComputeError>,
) -> Result<(), ComputeError> {
    let positions = csr.seek(row, hint);
    let columns = csr.targets_at(positions.clone());
    match csr.weights_at(positions).filter(|_| weighted) {
        Some(weights) => {
            for (&column, &weight) in columns.iter().zip(weights) {
                entry(column.dense(), weight)?;
            }
        }
        None => {
            for &column in columns {
                entry(column.dense(), 1.0)?;
            }
        }
    }
    Ok(())
}
