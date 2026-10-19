//! The id map: stable dense ids for external ids across versions of a graph
//! (DESIGN.md §5.2).

use std::cmp::Ordering;
use std::fmt::Debug;

use super::{memory, DenseId, GraphError, Tombstones};

/// An external id type: the type of a graph's node ids in its user tables
/// (`id_type`). Dense ids follow this type's order, which must match the
/// Postgres sort order of the column type.
pub trait ExternalId: Copy + Ord + Debug {}

/// `int4`.
impl ExternalId for i32 {}
/// `int8`.
impl ExternalId for i64 {}
/// `uuid`: Postgres compares uuids bytewise, as `[u8; 16]` does.
impl ExternalId for [u8; 16] {}

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
    pub fn rebuild(&self, mut externals: Vec<E>) -> Result<Self, GraphError> {
        externals.sort_unstable();
        externals.dedup();
        let mut by_external = memory::with_capacity(self.by_external.len() + externals.len())?;
        let mut fresh = Vec::new();
        let mut tombstones = self.tombstones.clone();
        let mut next_dense = self.id_space();
        let mut previous = self.by_external.iter().peekable();
        let mut current = externals.into_iter().peekable();
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
                        by_external.push((old, dense));
                    }
                }
                Ordering::Equal => {
                    if let Some(&(old, dense)) = previous.next() {
                        tombstones.remove(dense);
                        by_external.push((old, dense));
                    }
                    current.next();
                }
                Ordering::Greater => {
                    if let Some(new) = current.next() {
                        by_external.push((new, next_dense));
                        fresh.push(new);
                        next_dense += 1;
                    }
                }
            }
        }
        let mut by_dense = memory::with_capacity(self.by_dense.len() + fresh.len())?;
        by_dense.extend_from_slice(&self.by_dense);
        by_dense.extend(fresh);
        tombstones.grow_to(next_dense)?;
        Ok(Self { by_external, by_dense, tombstones })
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
