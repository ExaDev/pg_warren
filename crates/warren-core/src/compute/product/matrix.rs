//! The adjacency matrix a product reads: the CSRs of the selected relations
//! in the orientation the product needs, with entries summed over
//! relations.

use super::super::ComputeError;
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
        Ok(Self { csrs, weighted: weighted.unwrap_or(true) && has_weights, form })
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
        scratch: &mut Vec<(DenseId, f64)>,
        mut entry: impl FnMut(DenseId, f64) -> Result<(), ComputeError>,
    ) -> Result<(), ComputeError> {
        match self.csrs.as_slice() {
            [AnyCsr::U32(csr)] => entries(csr, row, self.weighted, entry),
            [AnyCsr::U64(csr)] => entries(csr, row, self.weighted, entry),
            many => {
                scratch.clear();
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
