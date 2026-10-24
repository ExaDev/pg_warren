//! Sorting each row by target and folding parallel edges, in place
//! (DESIGN.md §5.4 step 4, §14): a relation's parallel edges become one,
//! their weights summed in input order and counted as deduplicated edges;
//! in the merged CSR a target reached through several relations appears
//! once, its relations' summed weights summed again in relation order.
//! Weighted rows are sorted stably in a scratch buffer of the longest row,
//! charged twice over for the sort's own buffer.

use std::mem::size_of;

use super::super::{Csr, Target};
use super::columns::Rows;
use crate::graph::build_memory::bytes_of;
use crate::graph::{BuildBuffer, BuildCharge, BuildMemory, GraphError, RelationIndex};

impl<T: Target> Rows<T> {
    /// A relation's forward CSR from its rows, with the build memory it
    /// holds and the number of parallel edges folded.
    pub(super) fn into_relation_csr(
        mut self,
        id_space: u64,
    ) -> Result<(Csr<T>, BuildCharge, u64), GraphError> {
        let memory = self.offsets.memory().clone();
        let folded = match self.weights.take() {
            Some(mut weights) => {
                let folded = self.fold_weighted(&mut weights, &memory)?;
                self.weights = Some(weights);
                folded
            }
            None => self.fold_unweighted(),
        };
        let (csr, charge) = self.into_csr(id_space)?;
        Ok((csr, charge, folded))
    }

    /// The merged CSR from the rows of every edge, with the build memory it
    /// holds.
    pub(super) fn into_merged_csr(
        mut self,
        id_space: u64,
    ) -> Result<(Csr<T>, BuildCharge), GraphError> {
        let memory = self.offsets.memory().clone();
        match self.weights.take() {
            Some(mut weights) => {
                self.merge_weighted(&mut weights, &memory)?;
                self.weights = Some(weights);
            }
            None => _ = self.fold_unweighted(),
        }
        if let Some(mut relations) = self.relations.take() {
            relations.release();
        }
        self.into_csr(id_space)
    }

    /// The longest row.
    fn longest_row(&self) -> usize {
        self.offsets.windows(2).map(|row| (row[1] - row[0]) as usize).max().unwrap_or(0)
    }

    /// Sorts each row's targets and drops repeats; returns how many.
    fn fold_unweighted(&mut self) -> u64 {
        let mut folded = 0;
        self.compact(|targets, row_start, start, end, write| {
            targets[start..end].sort_unstable();
            for entry in start..end {
                let target = targets[entry];
                match *write > row_start && targets[*write - 1] == target {
                    true => folded += 1,
                    false => {
                        targets[*write] = target;
                        *write += 1;
                    }
                }
            }
        });
        self.targets.truncate(self.offsets.last().copied().unwrap_or(0) as usize);
        folded
    }

    /// Sorts each row stably by target and folds repeats into the first,
    /// summing weights in input order; returns how many were folded.
    fn fold_weighted(
        &mut self,
        weights: &mut BuildBuffer<f64>,
        memory: &BuildMemory,
    ) -> Result<u64, GraphError> {
        let longest = self.longest_row();
        let mut scratch = BuildBuffer::<(T, f64)>::with_capacity(memory, longest)?;
        let _sort = memory.charge(bytes_of::<(T, f64)>(longest))?;
        let mut folded = 0;
        self.compact(|targets, row_start, start, end, write| {
            scratch.truncate(0);
            scratch.extend((start..end).map(|entry| (targets[entry], weights[entry])));
            scratch.sort_by_key(|&(target, _)| target);
            for &(target, weight) in scratch.iter() {
                match *write > row_start && targets[*write - 1] == target {
                    true => {
                        weights[*write - 1] += weight;
                        folded += 1;
                    }
                    false => {
                        targets[*write] = target;
                        weights[*write] = weight;
                        *write += 1;
                    }
                }
            }
        });
        let edges = self.offsets.last().copied().unwrap_or(0) as usize;
        self.targets.truncate(edges);
        weights.truncate(edges);
        Ok(folded)
    }

    /// Sorts each row stably by (target, relation) and folds each target
    /// into one entry: weights summed per relation in input order, then
    /// across relations in relation order, as the merged CSR has them.
    fn merge_weighted(
        &mut self,
        weights: &mut BuildBuffer<f64>,
        memory: &BuildMemory,
    ) -> Result<(), GraphError> {
        let longest = self.longest_row();
        let mut scratch = BuildBuffer::<(T, RelationIndex, f64)>::with_capacity(memory, longest)?;
        let _sort = memory.charge(bytes_of::<(T, RelationIndex, f64)>(longest))?;
        let relations = self.relations.take();
        let relation =
            |entry: usize| relations.as_ref().map_or_else(Default::default, |r| r[entry]);
        self.compact(|targets, _, start, end, write| {
            scratch.truncate(0);
            scratch.extend(
                (start..end).map(|entry| (targets[entry], relation(entry), weights[entry])),
            );
            scratch.sort_by_key(|&(target, relation, _)| (target, relation));
            let mut entry = 0;
            while entry < scratch.len() {
                let target = scratch[entry].0;
                let mut total = relation_sum(&scratch, &mut entry);
                while entry < scratch.len() && scratch[entry].0 == target {
                    total += relation_sum(&scratch, &mut entry);
                }
                targets[*write] = target;
                weights[*write] = total;
                *write += 1;
            }
        });
        self.relations = relations;
        let edges = self.offsets.last().copied().unwrap_or(0) as usize;
        self.targets.truncate(edges);
        weights.truncate(edges);
        Ok(())
    }

    /// Rewrites every row in place, front to back: `each(targets, row
    /// start, input start, input end, write)` writes the row's entries from
    /// `row start` on, advancing `write`, which never passes the input it
    /// has yet to read. The offsets then follow the rewritten rows.
    fn compact(&mut self, mut each: impl FnMut(&mut [T], usize, usize, usize, &mut usize)) {
        let (mut start, mut write) = (0, 0);
        for row in 1..self.offsets.len() {
            let end = self.offsets[row] as usize;
            let row_start = write;
            each(&mut self.targets, row_start, start, end, &mut write);
            self.offsets[row] = write as u64;
            start = end;
        }
    }

    /// The CSR of the rewritten rows, its buffers shrunk to fit; deduplication
    /// empties no row, so sparse offsets still store only non-empty rows.
    fn into_csr(self, id_space: u64) -> Result<(Csr<T>, BuildCharge), GraphError> {
        let Self { rows, offsets, mut targets, weights, .. } = self;
        targets.shrink_to_fit();
        let (offsets, mut charge) = offsets.into_parts();
        let rows = rows.map(|rows| {
            let (rows, rows_charge) = rows.into_parts();
            charge.absorb(rows_charge);
            rows
        });
        let (targets, targets_charge) = targets.into_parts();
        charge.absorb(targets_charge);
        let weights = match weights {
            Some(mut weights) => {
                weights.shrink_to_fit();
                let (weights, weights_charge) = weights.into_parts();
                charge.absorb(weights_charge);
                Some(weights)
            }
            None => None,
        };
        let csr = match rows {
            Some(rows) => Csr::from_sparse_parts(id_space, rows, offsets, targets, weights),
            None => Csr::from_parts(id_space, offsets, targets, weights),
        };
        Ok((csr?, charge))
    }
}

/// The sum of the run of entries from `entry` with its target and
/// relation, in order; advances `entry` past the run.
fn relation_sum<T: Target>(entries: &[(T, RelationIndex, f64)], entry: &mut usize) -> f64 {
    let (target, relation, mut sum) = entries[*entry];
    *entry += 1;
    while *entry < entries.len() && (entries[*entry].0, entries[*entry].1) == (target, relation) {
        sum += entries[*entry].2;
        *entry += 1;
    }
    sum
}

/// The bytes of a weighted relation row entry in the scratch buffer.
pub(super) fn relation_entry<T>() -> u64 {
    size_of::<(T, f64)>() as u64
}

/// The bytes of a weighted merged row entry in the scratch buffer.
pub(super) fn merged_entry<T>() -> u64 {
    size_of::<(T, RelationIndex, f64)>() as u64
}
