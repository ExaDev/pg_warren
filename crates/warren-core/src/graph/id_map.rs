//! The id map: stable dense ids for external ids across versions of a graph
//! (DESIGN.md §5.2).

use std::cmp::Ordering;
use std::fmt::Debug;

use super::{
    memory, BuildBuffer, BuildCharge, BuildMemory, DenseId, GraphError, Permutation, Tombstones,
};

/// An external id type: the type of a graph's node ids in its user tables
/// (`id_type`). Dense ids follow this type's order, which must match the
/// Postgres sort order of the column type.
pub trait ExternalId: Copy + Ord + Debug {
    /// The encoded width in bytes.
    const WIDTH: usize;

    /// Appends the segment encoding of `self` (little-endian for integers).
    fn write_le(&self, out: &mut Vec<u8>);

    /// Decodes an id from exactly [`Self::WIDTH`] bytes.
    fn read_le(bytes: &[u8]) -> Self;
}

macro_rules! integer_external_id {
    ($type:ty, $doc:literal) => {
        #[doc = $doc]
        impl ExternalId for $type {
            const WIDTH: usize = std::mem::size_of::<$type>();

            fn write_le(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }

            fn read_le(bytes: &[u8]) -> Self {
                let mut array = [0; std::mem::size_of::<$type>()];
                array.copy_from_slice(bytes);
                <$type>::from_le_bytes(array)
            }
        }
    };
}

integer_external_id!(i32, "`int4`.");
integer_external_id!(i64, "`int8`.");

/// `uuid`, in its 16 bytes: Postgres compares uuids bytewise, as `[u8; 16]`
/// does.
impl ExternalId for [u8; 16] {
    const WIDTH: usize = 16;

    fn write_le(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self);
    }

    fn read_le(bytes: &[u8]) -> Self {
        let mut uuid = [0; 16];
        uuid.copy_from_slice(bytes);
        uuid
    }
}

/// The id map of one graph version: (external id, dense id) pairs sorted by
/// external id, an inverse array from dense id to external id, and the
/// tombstones of dense ids whose nodes have disappeared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdMap<E> {
    by_external: Vec<(E, DenseId)>,
    by_dense: Vec<E>,
    tombstones: Tombstones,
}

impl<E: ExternalId> Default for IdMap<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: ExternalId> IdMap<E> {
    /// The empty id map that precedes a graph's first build.
    pub fn new() -> Self {
        Self { by_external: Vec::new(), by_dense: Vec::new(), tombstones: Tombstones::default() }
    }

    /// The id map of the next build, whose live nodes are `externals`
    /// (duplicates allowed). Nodes in both keep their dense id; new nodes are
    /// appended at the high-water mark in external-id order; nodes that have
    /// disappeared are tombstoned. Dense ids are never reused, so a node that
    /// reappears after being tombstoned gets its old dense id back.
    pub fn rebuild(&self, externals: Vec<E>) -> Result<Self, GraphError> {
        let externals = BuildBuffer::from_vec(&BuildMemory::unlimited(), externals);
        Ok(self.rebuild_within(externals)?.0)
    }

    /// [`Self::rebuild`], charging every buffer to the memory `externals`
    /// is charged to; returns the charge for the id map it holds. The
    /// externals are sorted in place, the new ids among them counted, and
    /// every buffer reserved exactly; they are freed before the inverse map
    /// is built.
    pub fn rebuild_within(
        &self,
        mut externals: BuildBuffer<E>,
    ) -> Result<(Self, BuildCharge), GraphError> {
        let memory = externals.memory().clone();
        externals.sort_unstable();
        externals.dedup();
        let fresh_count = self.fresh(&externals);
        let mut by_external =
            BuildBuffer::with_capacity(&memory, self.by_external.len() + fresh_count)?;
        let mut fresh = BuildBuffer::with_capacity(&memory, fresh_count)?;
        let words = Tombstones::bytes_for(self.id_space() + fresh_count as u64);
        let tombstones_charge = memory.charge(words)?;
        let mut tombstones = self.tombstones.try_clone()?;
        let mut next_dense = self.id_space();
        let mut previous = self.by_external.iter().peekable();
        let mut current = externals.iter().copied().peekable();
        loop {
            let order = match (previous.peek(), current.peek()) {
                (None, None) => break,
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some((old, _)), Some(new)) => old.cmp(new),
            };
            match order {
                Ordering::Less => {
                    if let Some(&(old, dense)) = previous.next() {
                        tombstones.insert(dense);
                        by_external.push((old, dense))?;
                    }
                }
                Ordering::Equal => {
                    if let Some(&(old, dense)) = previous.next() {
                        tombstones.remove(dense);
                        by_external.push((old, dense))?;
                    }
                    current.next();
                }
                Ordering::Greater => {
                    if let Some(new) = current.next() {
                        by_external.push((new, next_dense))?;
                        fresh.push(new)?;
                        next_dense += 1;
                    }
                }
            }
        }
        drop(externals);
        let mut by_dense = BuildBuffer::with_capacity(&memory, self.by_dense.len() + fresh.len())?;
        by_dense.extend(self.by_dense.iter().copied());
        by_dense.extend(fresh.iter().copied());
        drop(fresh);
        tombstones.grow_to(next_dense)?;
        let (by_external, mut charge) = by_external.into_parts();
        let (by_dense, by_dense_charge) = by_dense.into_parts();
        charge.absorb(by_dense_charge);
        charge.absorb(tombstones_charge);
        Ok((Self { by_external, by_dense, tombstones }, charge))
    }

    /// How many of the sorted, distinct `externals` this map does not hold.
    fn fresh(&self, externals: &[E]) -> usize {
        let mut known = self.by_external.iter().map(|(external, _)| external).peekable();
        externals
            .iter()
            .filter(|external| {
                while known.next_if(|known| known < external).is_some() {}
                known.peek() != Some(external)
            })
            .count()
    }

    /// An id map from its (external id, dense id) pairs and tombstones,
    /// checking that the pairs are strictly sorted by external id, that the
    /// dense ids are exactly `0..id_space`, and that the tombstones cover
    /// the same id space.
    pub(crate) fn from_parts(
        by_external: Vec<(E, DenseId)>,
        tombstones: Tombstones,
    ) -> Result<Self, GraphError> {
        let id_space = by_external.len() as u64;
        if tombstones.id_space() != id_space {
            return Err(GraphError::TombstonesLength {
                expected: id_space,
                found: tombstones.id_space(),
            });
        }
        if by_external.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(GraphError::IdMapNotSorted);
        }
        let mut by_dense = memory::filled(by_external.len(), None)?;
        for &(external, dense) in &by_external {
            match usize::try_from(dense).ok().and_then(|d| by_dense.get_mut(d)) {
                Some(slot @ None) => *slot = Some(external),
                _ => return Err(GraphError::IdMapNotPermutation),
            }
        }
        let mut dense_to_external = memory::with_capacity(by_dense.len())?;
        for external in by_dense {
            dense_to_external.push(external.ok_or(GraphError::IdMapNotPermutation)?);
        }
        let by_dense = dense_to_external;
        Ok(Self { by_external, by_dense, tombstones })
    }

    /// Renumbers every dense id, tombstones included, by `permutation`,
    /// which must permute this id map's id space; else changes nothing.
    pub fn renumber(&mut self, permutation: &Permutation) -> Result<(), GraphError> {
        if permutation.id_space() != self.id_space() {
            return Err(GraphError::IdSpaceMismatch {
                expected: self.id_space(),
                found: permutation.id_space(),
            });
        }
        let mut tombstones = Tombstones::new(self.id_space())?;
        for old in (0..self.id_space()).filter(|&old| self.tombstones.contains(old)) {
            tombstones.insert(permutation.new_dense_id(old));
        }
        self.tombstones = tombstones;
        for (external, dense) in &mut self.by_external {
            *dense = permutation.new_dense_id(*dense);
            self.by_dense[*dense as usize] = *external;
        }
        Ok(())
    }

    /// The (external id, dense id) pairs, sorted by external id.
    pub(crate) fn pairs(&self) -> &[(E, DenseId)] {
        &self.by_external
    }

    /// The dense id of a live node, by binary search.
    pub fn dense_id(&self, external: &E) -> Option<DenseId> {
        let index =
            self.by_external.binary_search_by(|(candidate, _)| candidate.cmp(external)).ok()?;
        let dense = self.by_external[index].1;
        (!self.tombstones.contains(dense)).then_some(dense)
    }

    /// The external id that `dense` was assigned to, tombstoned or not.
    pub fn external_id(&self, dense: DenseId) -> Option<E> {
        usize::try_from(dense).ok().and_then(|index| self.by_dense.get(index)).copied()
    }

    /// The high-water mark of dense ids, tombstones included.
    pub fn id_space(&self) -> u64 {
        self.by_dense.len() as u64
    }

    /// The number of live nodes.
    pub fn node_count(&self) -> u64 {
        self.id_space() - self.tombstones.count()
    }

    /// The tombstones of this id map.
    pub fn tombstones(&self) -> &Tombstones {
        &self.tombstones
    }

    /// The heap bytes it holds: both directions of the map and the
    /// tombstones.
    pub fn bytes(&self) -> u64 {
        memory::held(&self.by_external) + memory::held(&self.by_dense) + self.tombstones.bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn first_build(externals: Vec<i64>) -> IdMap<i64> {
        IdMap::new().rebuild(externals).unwrap()
    }

    #[test]
    fn the_first_build_assigns_dense_ids_in_external_id_sort_order() {
        let id_map = first_build(vec![30, 10, 20, 10]);
        assert_eq!(id_map.id_space(), 3);
        assert_eq!(id_map.node_count(), 3);
        assert_eq!(id_map.dense_id(&10), Some(0));
        assert_eq!(id_map.dense_id(&20), Some(1));
        assert_eq!(id_map.dense_id(&30), Some(2));
        assert_eq!(id_map.external_id(2), Some(30));
    }

    #[test]
    fn an_id_map_holds_both_directions_and_its_tombstones() {
        let id_map = first_build(vec![30, 10, 20]);
        let pair = std::mem::size_of::<(i64, DenseId)>() as u64;
        assert_eq!(id_map.bytes(), 3 * pair + 3 * 8 + id_map.tombstones().bytes());
        assert_eq!(IdMap::<i64>::new().bytes(), 0);
    }

    /// A rebuild reserves exactly what the result holds: with a stable
    /// node set, its peak is the externals and the result, not a second
    /// copy of the base and a fresh-id buffer for every external.
    #[test]
    fn a_rebuild_charges_no_more_than_its_externals_and_its_result() {
        let nodes = 10_000i64;
        let base = first_build((0..nodes).collect());
        for externals in [(0..nodes).collect::<Vec<_>>(), (nodes / 2..nodes * 3 / 2).collect()] {
            let memory = BuildMemory::unlimited();
            let externals = BuildBuffer::from_vec(&memory, externals);
            let held = externals.bytes();
            let (rebuilt, charge) = base.rebuild_within(externals).unwrap();
            assert_eq!(charge.bytes(), rebuilt.bytes());
            assert!(
                memory.peak() <= held + rebuilt.bytes(),
                "{} > {held} + {}",
                memory.peak(),
                rebuilt.bytes()
            );
        }
    }

    /// The locality order of the path 0 - 2 - 1 (`new = [2, 0, 1]`), over
    /// an id space of 3.
    fn path_permutation() -> Permutation {
        let relation = crate::graph::RelationDictionary::new().intern("").unwrap();
        let mut edges = crate::graph::EdgeList::for_id_space(3, false);
        edges.push(relation, 0, 2, 1.0).unwrap();
        edges.push(relation, 2, 1, 1.0).unwrap();
        Permutation::locality(&edges, 3, &BuildMemory::unlimited()).unwrap()
    }

    #[test]
    fn renumbering_moves_both_directions_and_the_tombstones() {
        // 10 → 0, 20 → 1 (tombstoned), 30 → 2.
        let mut id_map = first_build(vec![10, 20, 30]).rebuild(vec![10, 30]).unwrap();
        id_map.renumber(&path_permutation()).unwrap();
        assert_eq!(id_map.dense_id(&10), Some(2));
        assert_eq!(id_map.dense_id(&30), Some(1));
        assert_eq!(id_map.dense_id(&20), None);
        assert_eq!(
            (0..3).map(|dense| id_map.external_id(dense).unwrap()).collect::<Vec<_>>(),
            [20, 30, 10]
        );
        assert!(id_map.tombstones().contains(0));
        assert_eq!(id_map.tombstones().count(), 1);
        assert_eq!((id_map.id_space(), id_map.node_count()), (3, 2));
        assert!(id_map.pairs().windows(2).all(|pair| pair[0].0 < pair[1].0));
    }

    #[test]
    fn renumbering_refuses_a_permutation_of_another_id_space() {
        let mut id_map = first_build(vec![10, 20]);
        assert_eq!(
            id_map.renumber(&path_permutation()),
            Err(GraphError::IdSpaceMismatch { expected: 2, found: 3 })
        );
        assert_eq!(id_map, first_build(vec![10, 20]));
    }

    #[test]
    fn unknown_ids_have_no_mapping() {
        let id_map = first_build(vec![1, 2]);
        assert_eq!(id_map.dense_id(&3), None);
        assert_eq!(id_map.external_id(2), None);
    }

    #[test]
    fn a_rebuild_keeps_survivors_appends_new_ids_and_tombstones_the_missing() {
        let previous = first_build(vec![10, 20, 30]);
        let id_map = previous.rebuild(vec![40, 5, 30, 10]).unwrap();
        assert_eq!(id_map.dense_id(&10), Some(0));
        assert_eq!(id_map.dense_id(&30), Some(2));
        assert_eq!(id_map.dense_id(&5), Some(3));
        assert_eq!(id_map.dense_id(&40), Some(4));
        assert_eq!(id_map.dense_id(&20), None);
        assert_eq!(id_map.external_id(1), Some(20));
        assert!(id_map.tombstones().contains(1));
        assert_eq!(id_map.id_space(), 5);
        assert_eq!(id_map.node_count(), 4);
    }

    #[test]
    fn a_resurrected_id_gets_its_old_dense_id_back() {
        let id_map =
            first_build(vec![10, 20]).rebuild(vec![10]).unwrap().rebuild(vec![10, 20]).unwrap();
        assert_eq!(id_map.dense_id(&20), Some(1));
        assert!(!id_map.tombstones().contains(1));
        assert_eq!(id_map.id_space(), 2);
    }

    #[test]
    fn uuids_sort_bytewise() {
        let low = [0u8; 16];
        let mut high = [0u8; 16];
        high[0] = 0xff;
        let id_map = IdMap::new().rebuild(vec![high, low]).unwrap();
        assert_eq!(id_map.dense_id(&low), Some(0));
        assert_eq!(id_map.dense_id(&high), Some(1));
    }

    fn builds() -> impl Strategy<Value = Vec<BTreeSet<i32>>> {
        prop::collection::vec(prop::collection::btree_set(0i32..40, 0..30), 1..8)
    }

    proptest! {
        #[test]
        fn dense_ids_are_stable_across_a_sequence_of_builds(builds in builds()) {
            let mut id_map = IdMap::<i32>::new();
            let mut ever: BTreeMap<i32, DenseId> = BTreeMap::new();
            for live in builds {
                let mut input: Vec<i32> = live.iter().copied().collect();
                input.extend(live.iter().take(3)); // duplicates
                input.reverse();
                let previous_id_space = id_map.id_space();
                id_map = id_map.rebuild(input).unwrap();

                prop_assert!(id_map.id_space() >= previous_id_space);
                prop_assert_eq!(id_map.node_count(), live.len() as u64);
                prop_assert_eq!(id_map.tombstones().count(), id_map.id_space() - live.len() as u64);
                let mut fresh = Vec::new();
                for external in &live {
                    let dense = id_map.dense_id(external);
                    prop_assert!(dense.is_some());
                    match ever.get(external) {
                        Some(&kept) => prop_assert_eq!(dense, Some(kept)),
                        None => fresh.push((*external, dense.unwrap_or_default())),
                    }
                }
                // New ids are appended at the high-water mark in sort order.
                for (offset, (external, dense)) in fresh.iter().enumerate() {
                    prop_assert_eq!(*dense, previous_id_space + offset as u64);
                    ever.insert(*external, *dense);
                }
                prop_assert_eq!(id_map.id_space(), ever.len() as u64);
                for (external, dense) in &ever {
                    prop_assert_eq!(id_map.external_id(*dense), Some(*external));
                    prop_assert_eq!(id_map.tombstones().contains(*dense), !live.contains(external));
                }
            }
        }
    }
}
