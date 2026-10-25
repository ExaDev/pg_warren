//! The adjacency matrix a product reads: the CSRs of the selected relations
//! in the orientation the product needs, an edge in several of them one
//! entry, its weights summed in relation order (1 when unweighted); or,
//! per relation, one entry in each. A cost reading (shortest and longest
//! paths, spanning forests) folds a pair's relations' entries into one,
//! its lightest or heaviest (DESIGN.md §5.3).

use std::mem::size_of;

use super::super::{ComputeError, ComputeMemory, MemoryCharge};
use super::folded::{Fold, FoldedRows};
use crate::graph::RelationSelection;
use crate::graph::{AnyCsr, AnyCsrSlices, CsrSlices, DenseId, GraphCsr, Target};
use crate::interrupts::{Cancellation, Work};

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
    id_space: u64,
    /// The rows of several CSRs folded into one entry per target, once
    /// [`Self::folded`] has folded them.
    folded: Option<FoldedRows>,
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
            id_space: graph.all_relations().forward().id_space(),
            folded: None,
        })
    }

    /// The forward CSRs a cost reading reads (shortest and longest paths,
    /// spanning forests), whose weights are lengths: weighted, every
    /// selected relation's own, an entry in each, for [`Self::folded`] to
    /// fold into a pair's lightest or heaviest, since two parallel edges
    /// are no longer a path than one of them (DESIGN.md §5.3); unweighted,
    /// the all-relations rows, a pair one entry of 1, one hop however many
    /// relations link it.
    pub(crate) fn costs(
        graph: &'g GraphCsr,
        relations: RelationSelection<'_>,
        weighted: Option<bool>,
        memory: &ComputeMemory,
    ) -> Result<Self, ComputeError> {
        let once = Self::new(graph, relations, false, weighted, false, memory)?;
        match once.weighted {
            true => Self::new(graph, relations, false, weighted, true, memory),
            false => Ok(once),
        }
    }

    /// The entries its CSRs store, a pair in several of them counted in
    /// each.
    pub(crate) fn stored_entries(&self) -> u64 {
        self.csrs.iter().map(AnyCsrSlices::edge_count).fold(0, u64::saturating_add)
    }

    /// Calls `edge(row, column, weight)` for each entry its CSRs store,
    /// CSR by CSR in relation order, rows ascending (weight 1 when
    /// unweighted), spending a unit of `work` per row and per entry.
    pub(crate) fn each_stored_entry(
        &self,
        work: &mut Work<'_>,
        edge: &mut dyn FnMut(DenseId, DenseId, f64),
    ) -> Result<(), ComputeError> {
        for csr in &self.csrs {
            match csr {
                AnyCsrSlices::U32(csr) => stored_entries(csr, self.weighted, work, edge)?,
                AnyCsrSlices::U64(csr) => stored_entries(csr, self.weighted, work, edge)?,
            }
        }
        Ok(())
    }

    /// The bytes [`Self::folded`] charges: none for the rows of one CSR.
    pub(crate) fn folded_bytes(&self) -> u64 {
        match self.csrs.len() {
            0 | 1 => 0,
            _ => FoldedRows::bytes(self.id_space, self.stored_entries()),
        }
    }

    /// The same matrix with the rows of several CSRs folded once into one
    /// entry per target, its weight `fold` of its entries' in relation
    /// order, so each later read of a row is one slice; the rows of one
    /// CSR are read as they are.
    pub(crate) fn folded(
        mut self,
        fold: Fold,
        cancellation: &Cancellation,
    ) -> Result<Self, ComputeError> {
        if self.csrs.len() < 2 {
            return Ok(self);
        }
        let each_edge = |edge: &mut dyn FnMut(DenseId, DenseId, f64)| {
            self.each_stored_entry(&mut Work::new(cancellation), edge)
        };
        let folded = FoldedRows::new(&self.memory, cancellation, self.id_space, each_edge, fold)?;
        self.folded = Some(folded);
        Ok(self)
    }

    /// A row scratch for [`Self::for_each_entry`], charged to the budget.
    pub(crate) fn scratch(&self) -> Result<RowScratch, ComputeError> {
        RowScratch::new(&self.memory)
    }

    /// Whether entries carry the edges' weights (else every weight is 1).
    pub(crate) fn is_weighted(&self) -> bool {
        self.weighted
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
        if let Some(folded) = &self.folded {
            return folded.row(row).iter().try_for_each(|&(column, weight)| entry(column, weight));
        }
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

/// Calls `edge(row, column, weight)` for each entry `csr` stores, rows
/// ascending (weight 1 unless `weighted`), spending a unit of `work` per
/// row and per entry.
fn stored_entries<T: Target>(
    csr: &CsrSlices<'_, T>,
    weighted: bool,
    work: &mut Work<'_>,
    edge: &mut dyn FnMut(DenseId, DenseId, f64),
) -> Result<(), ComputeError> {
    for (row, positions) in csr.stored_rows() {
        work.spend(1 + positions.len() as u64)?;
        let columns = csr.targets_at(positions.clone());
        match csr.weights_at(positions).filter(|_| weighted) {
            Some(weights) => {
                columns.iter().zip(weights).for_each(|(c, &w)| edge(row, c.dense(), w));
            }
            None => columns.iter().for_each(|c| edge(row, c.dense(), 1.0)),
        }
    }
    Ok(())
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
