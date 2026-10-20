//! The CSRs of one graph version: forward and (optionally) reverse per
//! relation, plus the merged all-relations CSR unless the graph merges the
//! relations' rows on the fly (DESIGN.md §5.3, §5.4 step 4).

use super::TargetWidth;
use super::{build, transpose::transpose, AllRelations, AnyCsr, Csr, MergedRows, Target};
use crate::graph::{EdgeList, GraphError, RelationIndex, RelationSelection};

/// The forward CSR of a relation, and its reverse if the graph builds one.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationCsr {
    forward: AnyCsr,
    reverse: Option<AnyCsr>,
}

impl RelationCsr {
    /// A relation's CSRs, checking that `reverse` has the same id space,
    /// target width, weightedness and edge count as `forward`. Debug builds
    /// also check that it is exactly the transpose.
    pub fn new(forward: AnyCsr, reverse: Option<AnyCsr>) -> Result<Self, GraphError> {
        if let Some(reverse) = &reverse {
            check_alike(&forward, reverse)?;
            if reverse.edge_count() != forward.edge_count() {
                return Err(GraphError::ReverseNotTranspose);
            }
            if cfg!(debug_assertions) && transposed(&forward)? != *reverse {
                return Err(GraphError::ReverseNotTranspose);
            }
        }
        Ok(Self { forward, reverse })
    }

    /// Out-edges: row `v` holds the targets of edges from `v`.
    pub fn forward(&self) -> &AnyCsr {
        &self.forward
    }

    /// In-edges: row `v` holds the sources of edges into `v`, if built.
    pub fn reverse(&self) -> Option<&AnyCsr> {
        self.reverse.as_ref()
    }

    fn csrs(&self) -> impl Iterator<Item = &AnyCsr> {
        std::iter::once(&self.forward).chain(&self.reverse)
    }

    /// The heap bytes its CSRs hold.
    pub fn bytes(&self) -> u64 {
        self.csrs().map(AnyCsr::bytes).sum()
    }
}

/// Whether a build of `relation_count` relations stores the merged CSR:
/// always without relations (its empty CSR carries the id space and the
/// reverse CSR's presence), never with one, else as `layout` says.
fn stores_merged(relation_count: usize, layout: CsrLayout) -> bool {
    match relation_count {
        0 => true,
        1 => false,
        _ => layout.store_merged,
    }
}

/// Checks that two CSRs of one graph version agree on id space, target
/// width and weightedness.
fn check_alike(expected: &AnyCsr, found: &AnyCsr) -> Result<(), GraphError> {
    if found.id_space() != expected.id_space() {
        return Err(GraphError::IdSpaceMismatch {
            expected: expected.id_space(),
            found: found.id_space(),
        });
    }
    if found.width() != expected.width() {
        return Err(GraphError::TargetWidthMismatch);
    }
    if found.is_weighted() != expected.is_weighted() {
        return Err(GraphError::WeightednessMismatch);
    }
    Ok(())
}

fn transposed(csr: &AnyCsr) -> Result<AnyCsr, GraphError> {
    Ok(match csr {
        AnyCsr::U32(csr) => transpose(csr)?.into(),
        AnyCsr::U64(csr) => transpose(csr)?.into(),
    })
}

/// How the relations and the all-relations rows are held, so that the
/// all-relations rows can always be read.
#[derive(Debug, Clone, PartialEq)]
enum Layout {
    /// Exactly one relation, which is also the all-relations CSR.
    Single(RelationCsr),
    /// Zero or several relations, plus the merged CSR.
    Merged { relations: Vec<RelationCsr>, merged: RelationCsr },
    /// Several relations, whose rows are merged on the fly.
    Unmerged { relations: Vec<RelationCsr> },
}

impl Layout {
    fn new(relations: Vec<RelationCsr>, merged: Option<RelationCsr>) -> Result<Self, GraphError> {
        let count = relations.len();
        match (merged, <[RelationCsr; 1]>::try_from(relations)) {
            (None, Ok([single])) => Ok(Self::Single(single)),
            (Some(merged), Err(relations)) => Ok(Self::Merged { relations, merged }),
            (None, Err(relations)) if count >= 2 => Ok(Self::Unmerged { relations }),
            _ => Err(GraphError::MergedCsrMismatch { relations: count }),
        }
    }
}

/// Which CSRs a build stores besides each relation's forward CSR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsrLayout {
    /// Build each relation's reverse CSR (by transposing the forward).
    pub build_reverse: bool,
    /// Store the merged CSR when there are several relations. Without it,
    /// all-relations reads merge the relations' rows on the fly
    /// ([`MergedRows`]), with the same results (DESIGN.md §5.3).
    pub store_merged: bool,
}

/// All the CSRs of one graph version.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphCsr {
    layout: Layout,
    deduplicated_edges: u64,
}

impl GraphCsr {
    /// The bytes of every offsets array a build allocates: `8·(id_space + 1)`
    /// per CSR, for each relation plus the merged CSR (when there are no
    /// relations, or several and `layout` stores it), twice with reverse
    /// CSRs. This term grows with relations × nodes, independently of the
    /// edge count (DESIGN.md §5.3). Saturates at `u64::MAX`.
    pub fn offsets_bytes(relation_count: usize, id_space: u64, layout: CsrLayout) -> u64 {
        let merged = usize::from(stores_merged(relation_count, layout));
        let directions: u64 = if layout.build_reverse { 2 } else { 1 };
        (relation_count as u64)
            .saturating_add(merged as u64)
            .saturating_mul(directions)
            .saturating_mul(id_space.saturating_add(1))
            .saturating_mul(8)
    }

    /// [`Self::build`], refused with [`GraphError::OffsetsOverBudget`] before
    /// anything is allocated if [`Self::offsets_bytes`] exceeds `budget`.
    pub fn build_within(
        edges: EdgeList,
        relation_count: usize,
        id_space: u64,
        layout: CsrLayout,
        budget: u64,
    ) -> Result<Self, GraphError> {
        let required = Self::offsets_bytes(relation_count, id_space, layout);
        if required > budget {
            return Err(GraphError::OffsetsOverBudget { required, budget });
        }
        Self::build_as(edges, relation_count, id_space, layout)
    }

    /// [`Self::build_as`], storing the merged CSR.
    pub fn build(
        edges: EdgeList,
        relation_count: usize,
        id_space: u64,
        build_reverse: bool,
    ) -> Result<Self, GraphError> {
        let layout = CsrLayout { build_reverse, store_merged: true };
        Self::build_as(edges, relation_count, id_space, layout)
    }

    /// Builds the CSRs of `edges`, whose relations index a dictionary of
    /// `relation_count` relations and whose dense ids lie in `id_space`,
    /// as `layout` says. Targets are `u32` when `id_space < 2³²`, else
    /// `u64` ([`TargetWidth`]). No memory budget applies beyond fallible
    /// allocation; the adapter uses [`Self::build_within`].
    pub fn build_as(
        edges: EdgeList,
        relation_count: usize,
        id_space: u64,
        layout: CsrLayout,
    ) -> Result<Self, GraphError> {
        match TargetWidth::for_id_space(id_space) {
            TargetWidth::U32 => Self::build_with::<u32>(edges, relation_count, id_space, layout),
            TargetWidth::U64 => Self::build_with::<u64>(edges, relation_count, id_space, layout),
        }
    }

    /// [`Self::build`] at target width `T`, which must cover the id space.
    /// Tests use it to reach the `u64` path without a 2³² id space.
    pub(crate) fn build_with<T: Target>(
        edges: EdgeList,
        relation_count: usize,
        id_space: u64,
        layout: CsrLayout,
    ) -> Result<Self, GraphError>
    where
        AnyCsr: From<Csr<T>>,
    {
        let store_merged = stores_merged(relation_count, layout);
        let forward = build::forward::<T>(edges, relation_count, id_space, store_merged)?;
        let with_reverse = |csr: Csr<T>| -> Result<RelationCsr, GraphError> {
            let reverse = match layout.build_reverse {
                true => Some(transpose(&csr)?.into()),
                false => None,
            };
            Ok(RelationCsr { forward: csr.into(), reverse })
        };
        let relations =
            forward.relations.into_iter().map(with_reverse).collect::<Result<Vec<_>, _>>()?;
        let merged = forward.merged.map(with_reverse).transpose()?;
        let layout = Layout::new(relations, merged)?;
        Ok(Self { layout, deduplicated_edges: forward.deduplicated_edges })
    }

    /// Assembles decoded CSRs into a graph, checking what the per-segment
    /// decoders cannot: every CSR has `id_space` (the id map's) and the
    /// target width it implies; all share weightedness; reverse CSRs are
    /// present for all or none; and `merged` is present when there are no
    /// relations, absent when there is exactly one, and either with
    /// several (absent: merged on the fly).
    pub fn from_parts(
        id_space: u64,
        relations: Vec<RelationCsr>,
        merged: Option<RelationCsr>,
        deduplicated_edges: u64,
    ) -> Result<Self, GraphError> {
        let layout = Layout::new(relations, merged)?;
        let graph = Self { layout, deduplicated_edges };
        let mut stored = graph.relations().iter().chain(graph.merged());
        let Some(first) = stored.next() else {
            return Err(GraphError::MergedCsrMismatch { relations: 0 });
        };
        if first.forward.id_space() != id_space {
            return Err(GraphError::IdSpaceMismatch {
                expected: id_space,
                found: first.forward.id_space(),
            });
        }
        if first.forward.width() != TargetWidth::for_id_space(id_space) {
            return Err(GraphError::TargetWidthMismatch);
        }
        for relation in graph.relations().iter().chain(graph.merged()) {
            if relation.reverse.is_some() != first.reverse.is_some() {
                return Err(GraphError::ReversePresenceMismatch);
            }
            for csr in relation.csrs() {
                check_alike(&first.forward, csr)?;
            }
        }
        Ok(graph)
    }

    /// The CSRs of each relation, in relation-index order.
    pub fn relations(&self) -> &[RelationCsr] {
        match &self.layout {
            Layout::Single(single) => std::slice::from_ref(single),
            Layout::Merged { relations, .. } | Layout::Unmerged { relations } => relations,
        }
    }

    /// The CSRs of one relation.
    pub fn relation(&self, relation: RelationIndex) -> Option<&RelationCsr> {
        self.relations().get(relation.index())
    }

    /// The CSRs `relations` selects: for every relation the stored
    /// all-relations CSRs, or every relation's when the merged CSR is not
    /// stored (readers of several CSRs merge them, as the merged CSR
    /// does); else each named relation once, in index order. The error is
    /// the smallest named relation that is not in the graph.
    pub fn selected(
        &self,
        relations: RelationSelection<'_>,
    ) -> Result<Vec<&RelationCsr>, RelationIndex> {
        match relations {
            RelationSelection::All => Ok(match &self.layout {
                Layout::Single(single) => vec![single],
                Layout::Merged { merged, .. } => vec![merged],
                Layout::Unmerged { relations } => relations.iter().collect(),
            }),
            RelationSelection::Only(indexes) => {
                let mut indexes = indexes.to_vec();
                indexes.sort_unstable();
                indexes.dedup();
                indexes
                    .into_iter()
                    .map(|relation| self.relation(relation).ok_or(relation))
                    .collect()
            }
        }
    }

    /// The merged all-relations CSRs, if stored: unless there is exactly
    /// one relation or the graph merges the relations' rows on the fly.
    pub fn merged(&self) -> Option<&RelationCsr> {
        match &self.layout {
            Layout::Single(_) | Layout::Unmerged { .. } => None,
            Layout::Merged { merged, .. } => Some(merged),
        }
    }

    /// The heap bytes every CSR holds, the merged CSRs included.
    pub fn bytes(&self) -> u64 {
        self.relations().iter().chain(self.merged()).map(RelationCsr::bytes).sum()
    }

    /// The all-relations rows in both directions: the stored merged CSR,
    /// the only relation's, or every relation's merged on the fly.
    pub fn all_relations(&self) -> AllRelations<'_> {
        fn read(csrs: &RelationCsr) -> AllRelations<'_> {
            AllRelations {
                forward: csrs.forward().into(),
                reverse: csrs.reverse().map(MergedRows::from),
            }
        }
        match &self.layout {
            Layout::Single(csrs) | Layout::Merged { merged: csrs, .. } => read(csrs),
            Layout::Unmerged { relations } => AllRelations {
                forward: MergedRows::merging(relations, false),
                reverse: relations
                    .iter()
                    .all(|relation| relation.reverse().is_some())
                    .then(|| MergedRows::merging(relations, true)),
            },
        }
    }

    /// The layout it was built with: whether it has reverse CSRs, and
    /// whether it stores its merged CSR (always without relations, never
    /// with one).
    pub fn layout(&self) -> CsrLayout {
        CsrLayout {
            build_reverse: self.all_relations().reverse().is_some(),
            store_merged: self.merged().is_some(),
        }
    }

    /// The number of parallel edges folded by deduplication.
    pub fn deduplicated_edges(&self) -> u64 {
        self.deduplicated_edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{
        DenseId, EdgeIntake, IdMap, RelationDictionary, RelationSelection, TargetWidth,
    };
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    type Naive = BTreeMap<(DenseId, DenseId), f64>;

    fn rows(csr: &AnyCsr) -> Naive {
        let mut edges = Naive::new();
        for source in 0..csr.id_space() {
            let weights = csr.neighbour_weights(source);
            for (i, target) in csr.neighbours(source).enumerate() {
                let weight = weights.map_or(1.0, |w| w[i]);
                edges.insert((source, target), weight);
            }
        }
        edges
    }

    fn pairs(rows: MergedRows<'_>) -> Vec<(DenseId, DenseId)> {
        let row = |source| rows.neighbours(source).map(move |target| (source, target));
        (0..rows.id_space()).flat_map(row).collect()
    }

    fn reversed(naive: &Naive) -> Naive {
        naive.iter().map(|(&(s, t), &w)| ((t, s), w)).collect()
    }

    fn two_relations() -> (RelationDictionary, RelationIndex, RelationIndex) {
        let mut relations = RelationDictionary::new();
        let a = relations.intern("a").unwrap();
        let b = relations.intern("b").unwrap();
        (relations, a, b)
    }

    #[test]
    fn parallel_edges_are_deduplicated_summing_weights_and_self_loops_kept() {
        let (_, a, _) = two_relations();
        let mut edges = EdgeList::new(true);
        edges.push(a, 2, 0, 1.0).unwrap();
        edges.push(a, 0, 1, 2.0).unwrap();
        edges.push(a, 2, 0, 3.0).unwrap();
        edges.push(a, 1, 1, 4.0).unwrap();
        edges.push(a, 2, 0, 5.0).unwrap();
        let graph = GraphCsr::build(edges, 1, 3, true).unwrap();
        let forward = graph.relations()[0].forward();
        assert_eq!(rows(forward), Naive::from([((0, 1), 2.0), ((1, 1), 4.0), ((2, 0), 9.0)]));
        assert_eq!(graph.deduplicated_edges(), 2);
        assert!(matches!(forward, AnyCsr::U32(_)));
    }

    #[test]
    fn relations_get_their_own_csr_and_the_merged_csr_unites_them() {
        let (_, a, b) = two_relations();
        let mut edges = EdgeList::new(true);
        edges.push(b, 0, 1, 1.0).unwrap();
        edges.push(a, 0, 1, 2.0).unwrap();
        edges.push(a, 1, 0, 3.0).unwrap();
        let graph = GraphCsr::build(edges, 2, 2, true).unwrap();
        assert_eq!(
            rows(graph.relation(a).unwrap().forward()),
            Naive::from([((0, 1), 2.0), ((1, 0), 3.0)])
        );
        assert_eq!(rows(graph.relation(b).unwrap().forward()), Naive::from([((0, 1), 1.0)]));
        let merged = graph.merged().unwrap();
        assert_eq!(rows(merged.forward()), Naive::from([((0, 1), 3.0), ((1, 0), 3.0)]));
        assert_eq!(rows(merged.reverse().unwrap()), Naive::from([((1, 0), 3.0), ((0, 1), 3.0)]));
        assert_eq!(graph.deduplicated_edges(), 0);
    }

    #[test]
    fn a_graph_holds_the_bytes_of_every_csr_in_both_directions() {
        let (_, a, b) = two_relations();
        let mut edges = EdgeList::new(true);
        edges.push(b, 0, 1, 1.0).unwrap();
        edges.push(a, 1, 0, 3.0).unwrap();
        let graph = GraphCsr::build(edges, 2, 2, true).unwrap();
        let relation =
            |csr: &RelationCsr| csr.forward().bytes() + csr.reverse().map_or(0, AnyCsr::bytes);
        let expected: u64 =
            graph.relations().iter().chain(graph.merged()).map(relation).sum::<u64>();
        assert_eq!(graph.bytes(), expected);
        assert!(graph.bytes() >= 3 * 2 * (3 * 8 + 4 + 8), "three CSRs and their reverses");
    }

    #[test]
    fn a_selection_names_the_all_relations_csrs_or_each_relation_once() {
        let (mut dictionary, a, b) = two_relations();
        let unknown = dictionary.intern("c").unwrap();
        let mut edges = EdgeList::new(false);
        edges.push(b, 0, 1, 1.0).unwrap();
        let graph = GraphCsr::build(edges, 2, 2, false).unwrap();
        assert_eq!(graph.selected(RelationSelection::All), Ok(vec![graph.merged().unwrap()]));
        let relation = |index| graph.relation(index).unwrap();
        assert_eq!(
            graph.selected(RelationSelection::Only(&[b, a, b])),
            Ok(vec![relation(a), relation(b)])
        );
        assert_eq!(graph.selected(RelationSelection::Only(&[])), Ok(vec![]));
        assert_eq!(graph.selected(RelationSelection::Only(&[unknown, a])), Err(unknown));
    }

    fn unmerged(reverse: bool) -> CsrLayout {
        CsrLayout { build_reverse: reverse, store_merged: false }
    }

    #[test]
    fn a_graph_may_leave_its_merged_csr_unstored() {
        let (_, a, b) = two_relations();
        let mut edges = EdgeList::new(true);
        edges.push(b, 0, 1, 1.0).unwrap();
        edges.push(a, 0, 1, 2.0).unwrap();
        edges.push(a, 1, 0, 3.0).unwrap();
        let stored = GraphCsr::build(edges.clone(), 2, 2, true).unwrap();
        let graph = GraphCsr::build_as(edges, 2, 2, unmerged(true)).unwrap();
        assert_eq!(graph.merged(), None);
        assert_eq!(graph.relations(), stored.relations());
        let merged = stored.merged().unwrap();
        assert_eq!(graph.bytes(), stored.bytes() - merged.bytes());
        let all = graph.all_relations();
        assert_eq!(pairs(all.forward()), pairs(merged.forward().into()));
        assert_eq!(all.reverse().map(pairs), merged.reverse().map(|r| pairs(r.into())));
        assert_eq!(
            graph.selected(RelationSelection::All),
            Ok(graph.relations().iter().collect::<Vec<_>>())
        );
    }

    #[test]
    fn one_relation_or_none_ignores_the_choice() {
        let (_, a, _) = two_relations();
        let mut edges = EdgeList::new(false);
        edges.push(a, 0, 1, 1.0).unwrap();
        let single = GraphCsr::build_as(edges.clone(), 1, 2, unmerged(false)).unwrap();
        assert_eq!(single, GraphCsr::build(edges, 1, 2, false).unwrap());
        let none = GraphCsr::build_as(EdgeList::new(false), 0, 3, unmerged(true)).unwrap();
        assert_eq!(none, GraphCsr::build(EdgeList::new(false), 0, 3, true).unwrap());
        assert!(none.merged().is_some(), "an empty graph keeps its empty merged CSR");
    }

    #[test]
    fn a_single_relation_is_its_own_all_relations_csr() {
        let (_, a, _) = two_relations();
        let mut edges = EdgeList::new(false);
        edges.push(a, 0, 1, 1.0).unwrap();
        let graph = GraphCsr::build(edges, 1, 2, false).unwrap();
        assert_eq!(graph.merged(), None);
        assert_eq!(graph.selected(RelationSelection::All), Ok(vec![&graph.relations()[0]]));
        let all = graph.all_relations();
        assert_eq!(pairs(all.forward()), pairs(graph.relations()[0].forward().into()));
        assert!(all.reverse().is_none());
    }

    #[test]
    fn a_graph_without_relations_has_an_empty_all_relations_csr() {
        let graph = GraphCsr::build(EdgeList::new(false), 0, 4, true).unwrap();
        assert!(graph.relations().is_empty());
        assert_eq!(graph.all_relations().forward().id_space(), 4);
        assert_eq!(graph.all_relations().forward().edge_count(), 0);
    }

    #[test]
    fn edges_outside_the_id_space_or_the_dictionary_are_errors() {
        let (_, a, b) = two_relations();
        let mut edges = EdgeList::new(false);
        edges.push(a, 0, 2, 1.0).unwrap();
        assert_eq!(
            GraphCsr::build(edges, 1, 2, false).unwrap_err(),
            GraphError::EdgeOutsideIdSpace { from: 0, to: 2 }
        );
        let mut edges = EdgeList::new(false);
        edges.push(b, 0, 1, 1.0).unwrap();
        assert_eq!(
            GraphCsr::build(edges, 1, 2, false).unwrap_err(),
            GraphError::RelationOutsideDictionary { relation: 1 }
        );
    }

    fn csr(id_space: u64, rows: &[&[u32]], weighted: bool) -> AnyCsr {
        let mut offsets = vec![0];
        let mut targets = Vec::new();
        for row in rows {
            targets.extend_from_slice(row);
            offsets.push(targets.len() as u64);
        }
        let weights = weighted.then(|| vec![1.0; targets.len()]);
        AnyCsr::U32(Csr::from_parts(id_space, offsets, targets, weights).unwrap())
    }

    fn relation(forward: AnyCsr, reverse: Option<AnyCsr>) -> RelationCsr {
        RelationCsr::new(forward, reverse).unwrap()
    }

    #[test]
    fn a_built_graph_reassembles_from_its_parts() {
        let (_, a, b) = two_relations();
        let mut edges = EdgeList::new(true);
        edges.push(a, 0, 1, 1.0).unwrap();
        edges.push(b, 1, 0, 2.0).unwrap();
        edges.push(b, 1, 0, 2.0).unwrap();
        let graph = GraphCsr::build(edges, 2, 2, true).unwrap();
        let parts = GraphCsr::from_parts(
            2,
            graph.relations().to_vec(),
            graph.merged().cloned(),
            graph.deduplicated_edges(),
        );
        assert_eq!(parts, Ok(graph));
    }

    #[test]
    fn a_relation_csr_needs_a_reverse_that_matches_its_forward() {
        let forward = csr(2, &[&[1], &[]], false);
        assert_eq!(
            RelationCsr::new(forward.clone(), Some(csr(3, &[&[], &[0], &[]], false))),
            Err(GraphError::IdSpaceMismatch { expected: 2, found: 3 })
        );
        assert_eq!(
            RelationCsr::new(forward.clone(), Some(csr(2, &[&[], &[0]], true))),
            Err(GraphError::WeightednessMismatch)
        );
        assert_eq!(
            RelationCsr::new(forward.clone(), Some(csr(2, &[&[], &[]], false))),
            Err(GraphError::ReverseNotTranspose)
        );
        let wide = AnyCsr::U64(Csr::from_parts(2, vec![0, 0, 1], vec![0u64], None).unwrap());
        assert_eq!(RelationCsr::new(forward, Some(wide)), Err(GraphError::TargetWidthMismatch));
    }

    #[test]
    fn from_parts_checks_the_cross_segment_invariants() {
        let one = || relation(csr(2, &[&[1], &[]], false), None);
        let check = |relations: Vec<RelationCsr>, merged: Option<RelationCsr>| {
            GraphCsr::from_parts(2, relations, merged, 0)
        };
        assert_eq!(
            check(vec![one()], Some(one())),
            Err(GraphError::MergedCsrMismatch { relations: 1 })
        );
        // Several relations without a merged CSR are merged on the fly.
        assert!(check(vec![one(), one()], None).is_ok_and(|graph| graph.merged().is_none()));
        assert_eq!(check(vec![], None), Err(GraphError::MergedCsrMismatch { relations: 0 }));
        assert_eq!(
            GraphCsr::from_parts(3, vec![one()], None, 0),
            Err(GraphError::IdSpaceMismatch { expected: 3, found: 2 })
        );
        let wide = RelationCsr::new(
            AnyCsr::U64(Csr::from_parts(2, vec![0, 0, 0], vec![], None).unwrap()),
            None,
        )
        .unwrap();
        assert_eq!(check(vec![wide], None), Err(GraphError::TargetWidthMismatch));
        let weighted = relation(csr(2, &[&[1], &[]], true), None);
        assert_eq!(
            check(vec![one(), weighted], Some(one())),
            Err(GraphError::WeightednessMismatch)
        );
        let reversed = relation(csr(2, &[&[1], &[]], false), Some(csr(2, &[&[], &[0]], false)));
        assert_eq!(
            check(vec![one(), reversed], Some(one())),
            Err(GraphError::ReversePresenceMismatch)
        );
    }

    #[test]
    fn from_parts_without_relations_takes_an_empty_merged_csr() {
        let empty = relation(csr(2, &[&[], &[]], false), None);
        let graph = GraphCsr::from_parts(2, vec![], Some(empty.clone()), 0).unwrap();
        assert!(graph.relations().is_empty());
        assert_eq!(graph.merged(), Some(&empty));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn debug_builds_check_that_reverse_is_the_transpose() {
        let forward = csr(2, &[&[1], &[]], false);
        assert_eq!(
            RelationCsr::new(forward, Some(csr(2, &[&[1], &[]], false))),
            Err(GraphError::ReverseNotTranspose)
        );
    }

    #[test]
    fn offsets_bytes_counts_every_offsets_array() {
        let stored = |build_reverse| CsrLayout { build_reverse, store_merged: true };
        // 1 relation, no merged CSR: 1 forward + 1 reverse array of 4 offsets.
        assert_eq!(GraphCsr::offsets_bytes(1, 3, stored(true)), 2 * 4 * 8);
        assert_eq!(GraphCsr::offsets_bytes(1, 3, unmerged(true)), 2 * 4 * 8);
        // 3 relations + merged, forward only; without the merged CSR.
        assert_eq!(GraphCsr::offsets_bytes(3, 3, stored(false)), 4 * 4 * 8);
        assert_eq!(GraphCsr::offsets_bytes(3, 3, unmerged(false)), 3 * 4 * 8);
        // No relations: the empty merged CSR alone.
        assert_eq!(GraphCsr::offsets_bytes(0, 3, unmerged(false)), 4 * 8);
        assert_eq!(GraphCsr::offsets_bytes(usize::MAX, u64::MAX - 1, stored(true)), u64::MAX);
    }

    #[test]
    fn a_build_over_its_offsets_budget_is_rejected_before_allocating() {
        let layout = CsrLayout { build_reverse: true, store_merged: true };
        let required = GraphCsr::offsets_bytes(1000, 1 << 40, layout);
        assert_eq!(
            GraphCsr::build_within(EdgeList::new(false), 1000, 1 << 40, layout, 1 << 30),
            Err(GraphError::OffsetsOverBudget { required, budget: 1 << 30 })
        );
        let budget = GraphCsr::offsets_bytes(2, 3, layout);
        assert!(GraphCsr::build_within(EdgeList::new(false), 2, 3, layout, budget).is_ok());
        // Without the merged CSR the same budget leaves room for a third relation.
        assert!(GraphCsr::build_within(EdgeList::new(false), 3, 3, unmerged(true), budget).is_ok());
    }

    #[derive(Debug, Clone)]
    struct Scenario {
        wide: bool,
        store_merged: bool,
        relation_count: usize,
        id_space: u64,
        weighted: bool,
        edges: Vec<(usize, DenseId, DenseId, u8)>,
    }

    fn scenarios() -> impl Strategy<Value = Scenario> {
        (any::<bool>(), any::<bool>(), 1usize..5, 1u64..12, any::<bool>()).prop_flat_map(
            |(wide, store_merged, relation_count, id_space, weighted)| {
                let edge = (0..relation_count, 0..id_space, 0..id_space, 0u8..8);
                prop::collection::vec(edge, 0..60).prop_map(move |edges| Scenario {
                    wide,
                    store_merged,
                    relation_count,
                    id_space,
                    weighted,
                    edges,
                })
            },
        )
    }

    proptest! {
        #[test]
        fn build_matches_a_naive_reference(scenario in scenarios()) {
            let mut relations = RelationDictionary::new();
            let indexes: Vec<RelationIndex> = (0..scenario.relation_count)
                .map(|r| relations.intern(&r.to_string()).unwrap())
                .collect();
            let mut edges = EdgeList::new(scenario.weighted);
            let mut per_relation = vec![Naive::new(); scenario.relation_count];
            let mut merged = Naive::new();
            for &(r, source, target, weight) in &scenario.edges {
                let weight = if scenario.weighted { f64::from(weight) } else { 1.0 };
                edges.push(indexes[r], source, target, weight).unwrap();
                let entry = per_relation[r].entry((source, target)).or_insert(0.0);
                *entry = if scenario.weighted { *entry + weight } else { 1.0 };
            }
            for naive in &per_relation {
                for (&key, &weight) in naive {
                    let entry = merged.entry(key).or_insert(0.0);
                    *entry = if scenario.weighted { *entry + weight } else { 1.0 };
                }
            }
            let distinct: usize = per_relation.iter().map(BTreeMap::len).sum();

            // The u64 path is otherwise reachable only from id spaces of 2^32.
            let layout = CsrLayout { build_reverse: true, store_merged: scenario.store_merged };
            let (count, id_space) = (scenario.relation_count, scenario.id_space);
            let graph = match scenario.wide {
                false => GraphCsr::build_as(edges, count, id_space, layout),
                true => GraphCsr::build_with::<u64>(edges, count, id_space, layout),
            }
            .unwrap();
            let width = if scenario.wide { TargetWidth::U64 } else { TargetWidth::U32 };
            prop_assert_eq!(graph.relations()[0].forward().width(), width);
            prop_assert_eq!(graph.merged().is_some(), scenario.store_merged && count > 1);

            prop_assert_eq!(graph.deduplicated_edges(), (scenario.edges.len() - distinct) as u64);
            for (relation, naive) in graph.relations().iter().zip(&per_relation) {
                prop_assert_eq!(relation.forward().id_space(), scenario.id_space);
                prop_assert_eq!(relation.forward().is_weighted(), scenario.weighted);
                prop_assert_eq!(&rows(relation.forward()), naive);
                prop_assert_eq!(rows(relation.reverse().unwrap()), reversed(naive));
            }
            // The weights of the stored all-relations CSR, if there is one.
            if let Some(stored) = graph.merged().or((count == 1).then(|| &graph.relations()[0])) {
                prop_assert_eq!(&rows(stored.forward()), &merged);
                prop_assert_eq!(rows(stored.reverse().unwrap()), reversed(&merged));
            }
            let all = graph.all_relations();
            let keys: Vec<_> = merged.keys().copied().collect();
            prop_assert_eq!(pairs(all.forward()), keys);
            let mut reverse_keys: Vec<_> = reversed(&merged).into_keys().collect();
            reverse_keys.sort_unstable();
            prop_assert_eq!(pairs(all.reverse().unwrap()), reverse_keys);
        }

        #[test]
        fn rows_merged_on_the_fly_read_as_the_stored_merged_csr(scenario in scenarios()) {
            let mut relations = RelationDictionary::new();
            let indexes: Vec<RelationIndex> = (0..scenario.relation_count)
                .map(|r| relations.intern(&r.to_string()).unwrap())
                .collect();
            let mut edges = EdgeList::new(scenario.weighted);
            for &(r, source, target, weight) in &scenario.edges {
                edges.push(indexes[r], source, target, f64::from(weight)).unwrap();
            }
            let (count, id_space) = (scenario.relation_count, scenario.id_space);
            let build = |store_merged| {
                let layout = CsrLayout { build_reverse: true, store_merged };
                GraphCsr::build_as(edges.clone(), count, id_space, layout).unwrap()
            };
            let (stored, unmerged) = (build(true), build(false));
            let (stored, unmerged) = (stored.all_relations(), unmerged.all_relations());
            for (stored, unmerged) in [
                (stored.forward(), unmerged.forward()),
                (stored.reverse().unwrap(), unmerged.reverse().unwrap()),
            ] {
                prop_assert_eq!(unmerged.id_space(), stored.id_space());
                prop_assert_eq!(unmerged.is_weighted(), stored.is_weighted());
                prop_assert_eq!(unmerged.edge_count(), stored.edge_count());
                prop_assert_eq!(pairs(unmerged), pairs(stored));
                for source in 0..=id_space {
                    prop_assert_eq!(unmerged.degree(source), stored.degree(source));
                    for target in 0..=id_space {
                        prop_assert_eq!(
                            unmerged.has_edge(source, target),
                            stored.has_edge(source, target)
                        );
                    }
                }
            }
        }

        #[test]
        fn the_pipeline_drops_dangling_edges_and_keeps_the_rest(
            nodes in prop::collection::btree_set(0i64..30, 0..20),
            rows_in in prop::collection::vec((0i64..30, 0i64..30, any::<bool>()), 0..60),
        ) {
            let id_map = IdMap::new().rebuild(nodes.iter().copied().collect()).unwrap();
            let mut relations = RelationDictionary::new();
            let r = relations.intern("r").unwrap();
            let mut intake = EdgeIntake::undirected(&id_map, false);
            let mut naive = Naive::new();
            let mut dangling = 0;
            for &(source, target, null_source) in &rows_in {
                let source_in = (!null_source).then_some(source);
                intake.push(r, source_in.as_ref(), Some(&target), 1.0).unwrap();
                match (source_in.and_then(|s| id_map.dense_id(&s)), id_map.dense_id(&target)) {
                    (Some(s), Some(t)) => {
                        naive.insert((s, t), 1.0);
                        naive.insert((t, s), 1.0);
                    }
                    _ => dangling += 1,
                }
            }
            let (edges, dropped) = intake.finish();
            prop_assert_eq!(dropped, dangling);
            let graph = GraphCsr::build(edges, relations.len(), id_map.id_space(), false).unwrap();
            prop_assert_eq!(rows(graph.relations()[0].forward()), naive);
        }
    }
}
