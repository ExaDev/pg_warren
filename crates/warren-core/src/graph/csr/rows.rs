//! CSR rows behind one read API: one CSR's, or the all-relations rows
//! (DESIGN.md §5.3) read from the stored merged CSR or merged on the fly
//! from every relation's CSR when the graph does not store it.

use std::iter::FusedIterator;

use super::merge::RowMerge;
use super::{AnyCsr, AnyCsrSlices, Csr, CsrSlices, Neighbours, RelationCsr, Target};
use crate::graph::{memory, DenseId, GraphError};

/// The rows of one CSR (a relation's, or the stored merged CSR), or the
/// all-relations rows merged on the fly from every relation's CSR in one
/// direction. Either way a row's targets are ascending and distinct: a
/// target reached through several relations appears once, as in the stored
/// merged CSR, so readers see the same rows whichever the graph holds.
#[derive(Debug, Clone, Copy)]
pub struct CsrRows<'g>(Source<'g>);

#[derive(Debug, Clone, Copy)]
enum Source<'g> {
    /// One CSR: the stored merged CSR, a graph's only relation, or one
    /// relation of a selection.
    Csr(&'g AnyCsr),
    /// Every relation's forward CSR, or reverse CSR if `reverse`; the
    /// graph checks that every relation has one.
    Relations { relations: &'g [RelationCsr], reverse: bool },
}

impl<'g> CsrRows<'g> {
    /// The forward or (if `reverse`) reverse CSRs of `relations`, merged as
    /// they are read. The caller ensures there are at least two relations
    /// and, for `reverse`, that each has a reverse CSR.
    pub(super) fn merging(relations: &'g [RelationCsr], reverse: bool) -> Self {
        Self(Source::Relations { relations, reverse })
    }

    fn csrs(&self) -> impl Iterator<Item = &'g AnyCsr> + 'g {
        let (one, relations, reverse) = match self.0 {
            Source::Csr(csr) => (Some(csr), &[][..], false),
            Source::Relations { relations, reverse } => (None, relations, reverse),
        };
        let merged = relations.iter().filter_map(move |relation| match reverse {
            true => relation.reverse(),
            false => Some(relation.forward()),
        });
        one.into_iter().chain(merged)
    }

    /// The number of rows: the id space of the graph version.
    pub fn id_space(&self) -> u64 {
        self.csrs().next().map_or(0, AnyCsr::id_space)
    }

    /// Whether the CSRs carry weights.
    pub fn is_weighted(&self) -> bool {
        self.csrs().next().is_some_and(AnyCsr::is_weighted)
    }

    /// The edges the CSRs read store, summed: [`Self::edge_count`] for one
    /// CSR, and at least it when merging on the fly (an edge in several
    /// relations counts once per relation). O(relations).
    pub(crate) fn stored_edges(&self) -> u64 {
        self.csrs().map(AnyCsr::edge_count).fold(0, u64::saturating_add)
    }

    /// The number of edges: distinct (source, target) pairs. O(1) for one
    /// CSR; merging on the fly, O(N + M).
    pub fn edge_count(&self) -> u64 {
        match self.0 {
            Source::Csr(csr) => csr.edge_count(),
            Source::Relations { .. } => (0..self.id_space()).map(|row| self.degree(row)).sum(),
        }
    }

    /// The targets of `source`'s row in ascending order, each once; empty
    /// outside the id space.
    pub fn neighbours(&self, source: DenseId) -> RowNeighbours<'g> {
        match self.0 {
            Source::Csr(csr) => RowNeighbours(Merge::One(csr.neighbours(source))),
            Source::Relations { .. } => RowNeighbours(Merge::Many(self.merge(source))),
        }
    }

    /// The merge of every CSR's `source` row. Every CSR of a graph version
    /// has one target width (checked by `GraphCsr::from_parts`), so no CSR
    /// is skipped.
    fn merge(&self, source: DenseId) -> Typed<'g> {
        fn rows<'g, T: Target>(
            csrs: impl Iterator<Item = Option<&'g Csr<T>>>,
            source: DenseId,
        ) -> RowMerge<'g, T> {
            RowMerge::new(csrs.flatten().map(|csr| csr.neighbours(source)))
        }
        match self.csrs().next() {
            Some(AnyCsr::U64(_)) => Typed::Wide(rows(self.csrs().map(wide), source)),
            _ => Typed::Narrow(rows(self.csrs().map(narrow), source)),
        }
    }

    /// The number of [`Self::neighbours`] of `source`.
    pub fn degree(&self, source: DenseId) -> u64 {
        match self.0 {
            Source::Csr(csr) => csr.degree(source),
            Source::Relations { .. } => self.neighbours(source).count() as u64,
        }
    }

    /// An upper bound on the length of `source`'s row, in O(1) per CSR
    /// read: its degree in one CSR, or the sum of its degrees in the
    /// relations merged on the fly (a target in several counts once in
    /// the merged row). Zero outside the id space.
    pub fn degree_bound(&self, source: DenseId) -> u64 {
        self.csrs().map(|csr| csr.degree(source)).sum()
    }

    /// Whether the edge `source → target` exists in any of the CSRs.
    pub fn has_edge(&self, source: DenseId, target: DenseId) -> bool {
        self.csrs().any(|csr| csr.has_edge(source, target))
    }

    /// These rows, for reading sources mostly in ascending order: each
    /// CSR's arrays borrowed once, with a hint per CSR (merging on the
    /// fly, a buffer of a few words per relation).
    pub(crate) fn sweep(&self) -> Result<RowSweep<'g>, GraphError> {
        fn hinted<'g, T: Target>(
            csrs: impl Iterator<Item = Option<&'g Csr<T>>>,
            count: usize,
        ) -> Result<Vec<Hinted<'g, T>>, GraphError> {
            let mut hinted = memory::with_capacity(count)?;
            hinted.extend(csrs.flatten().map(|csr| Hinted { slices: csr.slices(), hint: 0 }));
            Ok(hinted)
        }
        if let Source::Csr(csr) = self.0 {
            return Ok(RowSweep(Sweep::One(csr.slices(), 0)));
        }
        let count = self.csrs().count();
        Ok(RowSweep(match self.csrs().next() {
            Some(AnyCsr::U64(_)) => Sweep::Wide(hinted(self.csrs().map(wide), count)?),
            _ => Sweep::Narrow(hinted(self.csrs().map(narrow), count)?),
        }))
    }
}

fn narrow(csr: &AnyCsr) -> Option<&Csr<u32>> {
    match csr {
        AnyCsr::U32(csr) => Some(csr),
        AnyCsr::U64(_) => None,
    }
}

fn wide(csr: &AnyCsr) -> Option<&Csr<u64>> {
    match csr {
        AnyCsr::U64(csr) => Some(csr),
        AnyCsr::U32(_) => None,
    }
}

/// [`CsrRows`] read with sources mostly ascending, as a sweep or a sorted
/// frontier reads them: where a CSR's offsets are sparse, a row is sought
/// from where that CSR's last one was found ([`CsrSlices::seek`]), so
/// ascending sources cost O(1) a row rather than a binary search in each
/// relation. A source below the last one is still found, by a search from
/// the start. Every CSR of a graph version has one target width.
#[derive(Debug)]
pub(crate) struct RowSweep<'g>(Sweep<'g>);

#[derive(Debug)]
enum Sweep<'g> {
    One(AnyCsrSlices<'g>, usize),
    Narrow(Vec<Hinted<'g, u32>>),
    Wide(Vec<Hinted<'g, u64>>),
}

/// One CSR's arrays and where its last row was found.
#[derive(Debug)]
struct Hinted<'g, T> {
    slices: CsrSlices<'g, T>,
    hint: usize,
}

impl<'g, T: Target> Hinted<'g, T> {
    fn row(&mut self, source: DenseId) -> &'g [T] {
        self.slices.targets_at(self.slices.seek(source, &mut self.hint))
    }

    fn degree(&mut self, source: DenseId) -> u64 {
        self.slices.seek(source, &mut self.hint).len() as u64
    }
}

impl<'g> RowSweep<'g> {
    /// [`CsrRows::neighbours`].
    pub(crate) fn neighbours(&mut self, source: DenseId) -> RowNeighbours<'g> {
        RowNeighbours(match &mut self.0 {
            Sweep::One(csr, hint) => Merge::One(csr.neighbours_at(csr.seek(source, hint))),
            Sweep::Narrow(csrs) => {
                Merge::Many(Typed::Narrow(RowMerge::new(csrs.iter_mut().map(|c| c.row(source)))))
            }
            Sweep::Wide(csrs) => {
                Merge::Many(Typed::Wide(RowMerge::new(csrs.iter_mut().map(|c| c.row(source)))))
            }
        })
    }

    /// [`CsrRows::degree_bound`].
    pub(crate) fn degree_bound(&mut self, source: DenseId) -> u64 {
        match &mut self.0 {
            Sweep::One(csr, hint) => csr.seek(source, hint).len() as u64,
            Sweep::Narrow(csrs) => csrs.iter_mut().map(|csr| csr.degree(source)).sum(),
            Sweep::Wide(csrs) => csrs.iter_mut().map(|csr| csr.degree(source)).sum(),
        }
    }
}

/// The all-relations (merged) rows of a graph version, forward and (if the
/// graph builds reverse CSRs) reverse.
#[derive(Debug, Clone, Copy)]
pub struct AllRelations<'g> {
    pub(super) forward: CsrRows<'g>,
    pub(super) reverse: Option<CsrRows<'g>>,
}

impl<'g> AllRelations<'g> {
    /// Out-edges: row `v` holds the targets of edges from `v`.
    pub fn forward(&self) -> CsrRows<'g> {
        self.forward
    }

    /// In-edges: row `v` holds the sources of edges into `v`, if built.
    pub fn reverse(&self) -> Option<CsrRows<'g>> {
        self.reverse
    }
}

impl<'g> From<&'g AnyCsr> for CsrRows<'g> {
    fn from(csr: &'g AnyCsr) -> Self {
        Self(Source::Csr(csr))
    }
}

/// A merge of rows at the version's target width.
#[derive(Debug, Clone)]
enum Typed<'g> {
    Narrow(RowMerge<'g, u32>),
    Wide(RowMerge<'g, u64>),
}

#[derive(Debug, Clone)]
enum Merge<'g> {
    One(Neighbours<'g>),
    Many(Typed<'g>),
}

/// The targets of one row, as dense ids, ascending and distinct.
#[derive(Debug, Clone)]
pub struct RowNeighbours<'g>(Merge<'g>);

impl Iterator for RowNeighbours<'_> {
    type Item = DenseId;

    fn next(&mut self) -> Option<DenseId> {
        match &mut self.0 {
            Merge::One(row) => row.next(),
            Merge::Many(Typed::Narrow(rows)) => rows.next_target(),
            Merge::Many(Typed::Wide(rows)) => rows.next_target(),
        }
    }
}

impl FusedIterator for RowNeighbours<'_> {}

#[cfg(test)]
mod tests {
    use super::super::transpose::transpose;
    use super::*;
    use crate::graph::Csr;

    fn csr(rows: &[&[u32]]) -> Csr<u32> {
        let mut offsets = vec![0];
        let mut targets = Vec::new();
        for row in rows {
            targets.extend_from_slice(row);
            offsets.push(targets.len() as u64);
        }
        Csr::from_parts(rows.len() as u64, offsets, targets, None).unwrap()
    }

    fn relations() -> Vec<RelationCsr> {
        // r0: 0 → {1, 3}, 2 → {2}; r1: 0 → {0, 3}, 1 → {2}
        let r0 = csr(&[&[1, 3], &[], &[2], &[]]);
        let r1 = csr(&[&[0, 3], &[2], &[], &[]]);
        [r0, r1]
            .into_iter()
            .map(|forward| {
                let reverse = transpose(&forward).unwrap().into();
                RelationCsr::new(forward.into(), Some(reverse)).unwrap()
            })
            .collect()
    }

    fn rows(rows: CsrRows<'_>) -> Vec<Vec<DenseId>> {
        (0..rows.id_space()).map(|row| rows.neighbours(row).collect()).collect()
    }

    #[test]
    fn rows_merged_on_the_fly_are_ascending_and_distinct() {
        let relations = relations();
        let forward = CsrRows::merging(&relations, false);
        assert_eq!(rows(forward), [vec![0, 1, 3], vec![2], vec![2], vec![]]);
        assert_eq!(forward.id_space(), 4);
        assert_eq!(forward.edge_count(), 5);
        assert_eq!(forward.degree(0), 3);
        assert!(forward.has_edge(1, 2) && forward.has_edge(0, 0) && !forward.has_edge(3, 0));
        let reverse = CsrRows::merging(&relations, true);
        assert_eq!(rows(reverse), [vec![0], vec![0], vec![1, 2], vec![0]]);
        assert_eq!(reverse.edge_count(), 5);
        assert!(!forward.is_weighted());
    }

    #[test]
    fn rows_outside_the_id_space_are_empty() {
        let relations = relations();
        let forward = CsrRows::merging(&relations, false);
        assert_eq!(forward.neighbours(4).count(), 0);
        assert_eq!(forward.degree(u64::MAX), 0);
    }

    /// `csr` with sparse offsets: only its non-empty rows stored.
    fn sparse(csr: &Csr<u32>) -> Csr<u32> {
        let (mut rows, mut offsets, mut targets) = (Vec::new(), vec![0], Vec::new());
        for source in (0..csr.id_space()).filter(|&source| csr.degree(source) > 0) {
            rows.push(source as u32);
            targets.extend_from_slice(csr.neighbours(source));
            offsets.push(targets.len() as u64);
        }
        Csr::from_sparse_parts(csr.id_space(), rows, offsets, targets, None).unwrap()
    }

    #[test]
    fn a_sweep_reads_the_rows_lookups_read_in_any_order() {
        let r0 = csr(&[&[1, 3], &[], &[2], &[], &[], &[0]]);
        let r1 = csr(&[&[0, 3], &[2], &[], &[], &[5], &[]]);
        let relations: Vec<_> = [r0, r1]
            .iter()
            .map(|forward| {
                let reverse = transpose(forward).unwrap();
                RelationCsr::new(sparse(forward).into(), Some(sparse(&reverse).into())).unwrap()
            })
            .collect();
        let one = AnyCsr::from(sparse(&csr(&[&[1], &[], &[0, 1], &[]])));
        let order = [0, 1, 2, 3, 4, 5, 6, 5, 2, 0, 4, 4, 1, u64::MAX, 3];
        for reverse in [false, true] {
            for rows in [CsrRows::merging(&relations, reverse), CsrRows::from(&one)] {
                let mut sweep = rows.sweep().unwrap();
                for source in order {
                    assert_eq!(
                        sweep.neighbours(source).collect::<Vec<_>>(),
                        rows.neighbours(source).collect::<Vec<_>>(),
                    );
                    assert_eq!(sweep.degree_bound(source), rows.degree_bound(source));
                }
            }
        }
    }

    #[test]
    fn one_csr_reads_as_it_is() {
        let one = AnyCsr::from(csr(&[&[1], &[0, 1]]));
        let rows_of_one = CsrRows::from(&one);
        assert_eq!(rows(rows_of_one), [vec![1], vec![0, 1]]);
        assert_eq!((rows_of_one.edge_count(), rows_of_one.degree(1)), (3, 2));
    }
}
