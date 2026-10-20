//! The adjacency matrix a product reads: the CSRs of the selected relations
//! in the orientation the product needs, with entries summed over
//! relations.

use std::mem::size_of;

use super::super::{ComputeError, ComputeMemory, MemoryCharge};
use crate::graph::RelationSelection;
use crate::graph::{AnyCsr, Csr, DenseId, GraphCsr, Target};

/// How a product walks the matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Form {
    /// Row `i` of the CSRs read is row `i` of the result.
    Pull,
    /// Row `i` of the CSRs read scatters into the rows of its targets.
    Push,
}

/// The CSRs a product reads.
#[derive(Debug)]
pub(super) struct Matrix<'g> {
    csrs: Vec<&'g AnyCsr>,
    weighted: bool,
    form: Form,
    /// The budget row scratches are charged to.
    memory: ComputeMemory,
}

/// The entries of one row of several CSRs, sorted and folded: a buffer
/// reused across rows, its capacity charged to the compute budget before
/// it is reserved.
#[derive(Debug)]
pub(super) struct RowScratch {
    entries: Vec<(DenseId, f64)>,
    charge: MemoryCharge,
}

impl RowScratch {
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
    /// pulled, else the forward CSRs pushed.
    pub(super) fn new(
        graph: &'g GraphCsr,
        relations: RelationSelection<'_>,
        transpose: bool,
        weighted: Option<bool>,
        memory: &ComputeMemory,
    ) -> Result<Self, ComputeError> {
        let selected = graph
            .selected(relations)
            .map_err(|relation| ComputeError::UnknownRelation { relation: relation.index() })?;
        let reverse: Option<Vec<&AnyCsr>> = selected.iter().map(|r| r.reverse()).collect();
        let (csrs, form) = match (transpose, reverse) {
            (true, Some(reverse)) => (reverse, Form::Pull),
            (true, None) => (selected.iter().map(|r| r.forward()).collect(), Form::Push),
            (false, _) => (selected.iter().map(|r| r.forward()).collect(), Form::Pull),
        };
        let has_weights = csrs.iter().any(|csr| csr.is_weighted());
        Ok(Self {
            csrs,
            weighted: weighted.unwrap_or(true) && has_weights,
            form,
            memory: memory.clone(),
        })
    }

    /// A row scratch for [`Self::for_each_entry`], charged to the budget.
    pub(super) fn scratch(&self) -> Result<RowScratch, ComputeError> {
        Ok(RowScratch { entries: Vec::new(), charge: self.memory.charge(0)? })
    }

    /// How the product walks the CSRs.
    pub(super) fn form(&self) -> Form {
        self.form
    }

    /// Calls `entry(column, weight)` for each entry of `row` in ascending
    /// column order: an edge in several selected relations is one entry
    /// whose weight is their sum, and every weight is 1 when unweighted.
    /// `scratch` is reused across rows.
    pub(super) fn for_each_entry(
        &self,
        row: DenseId,
        scratch: &mut RowScratch,
        mut entry: impl FnMut(DenseId, f64) -> Result<(), ComputeError>,
    ) -> Result<(), ComputeError> {
        match self.csrs.as_slice() {
            [AnyCsr::U32(csr)] => entries(csr, row, self.weighted, entry),
            [AnyCsr::U64(csr)] => entries(csr, row, self.weighted, entry),
            many => {
                let len = many.iter().map(|csr| csr.degree(row) as usize).sum();
                let scratch = scratch.clear_for(len)?;
                for csr in many {
                    let weights = csr.neighbour_weights(row).filter(|_| self.weighted);
                    scratch.extend(csr.neighbours(row).enumerate().map(|(i, column)| {
                        (column, weights.and_then(|w| w.get(i)).copied().unwrap_or(1.0))
                    }));
                }
                scratch.sort_by_key(|&(column, _)| column);
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

/// The CSRs of one relation's row, weighted or with unit weights.
fn entries<T: Target>(
    csr: &Csr<T>,
    row: DenseId,
    weighted: bool,
    mut entry: impl FnMut(DenseId, f64) -> Result<(), ComputeError>,
) -> Result<(), ComputeError> {
    let columns = csr.neighbours(row);
    match csr.neighbour_weights(row).filter(|_| weighted) {
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
