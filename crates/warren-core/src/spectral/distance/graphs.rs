//! A version of another graph in a version's id space, by external id, for
//! comparing across graphs (DESIGN.md §8: the shadow-schema pattern).
//! Nodes of both keep this version's dense ids and nodes only in the other
//! are appended in external-id order; relations are matched by name, those
//! only in the other appended to this version's dictionary. The mapping is
//! O(N); nothing is rebuilt in the shared ids: vectors are moved by it
//! ([`super::align`]) and `edit` maps edges through it as it reads them
//! ([`super::aligned_edit_distance`]).

use super::super::{Resources, SpectralError};
use crate::compute::{ComputeError, MemoryCharge};
use crate::graph::{DenseId, IdKeys, RelationDictionary};
use crate::interrupts::Interrupts;
use crate::version::GraphVersion;

/// How often the passes poll the interrupts, in rows.
pub(super) const ROWS_PER_POLL: u64 = 1 << 16;

/// How another graph's version maps into this version's id space.
#[derive(Debug)]
pub struct IdAlignment {
    /// Entry `i` is the shared dense id of the other version's dense id `i`.
    pub dense_ids: Vec<DenseId>,
    /// The shared id space: this version's, plus the other's own nodes.
    pub id_space: u64,
    /// This version's relation dictionary, plus the other's own relations.
    pub relations: RelationDictionary,
    _charge: MemoryCharge,
}

/// How `other` maps into `this`'s id space by external id: a merge of the
/// two id maps in external-id order (for text ids, of their `id_order`s),
/// so O(N) with no lookup. A node tombstoned in `this` is `other`'s own.
pub fn align_ids<K: IdKeys, I: Interrupts>(
    resources: &mut Resources<'_, I>,
    this: &GraphVersion<'_, K>,
    other: &GraphVersion<'_, K>,
) -> Result<IdAlignment, SpectralError> {
    let (mut dense_ids, charge) = resources.memory.filled(other.id_map().id_space() as usize, 0)?;
    let mut id_space = this.id_map().id_space();
    let mut these = this.id_map().keys().in_order().peekable();
    for (row, (external, dense)) in other.id_map().keys().in_order().enumerate() {
        if (row as u64).is_multiple_of(ROWS_PER_POLL) {
            resources.interrupts.check().map_err(ComputeError::from)?;
        }
        while these.next_if(|&(this_external, _)| this_external < external).is_some() {}
        let shared = these
            .next_if(|&(this_external, _)| this_external == external)
            .map(|(_, this_dense)| this_dense)
            .filter(|&this_dense| !this.id_map().tombstones().contains(this_dense));
        dense_ids[dense as usize] = shared.unwrap_or_else(|| {
            id_space += 1;
            id_space - 1
        });
    }
    let mut relations = this.relations().clone();
    for name in other.relations().names() {
        relations.intern(name)?;
    }
    Ok(IdAlignment { dense_ids, id_space, relations, _charge: charge })
}

// Not under Miri: these tests build the compute pool, whose work stealing
// runs through crossbeam-epoch, which Miri's Stacked Borrows model rejects
// (see compute::workers).
#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;
    use crate::compute::{ComputeError, ComputeMemory, Workers};
    use crate::interrupts::Uninterrupted;
    use crate::version::history::{builds, ranks, Build};
    use proptest::prelude::*;

    fn with<R>(limit: u64, f: impl FnOnce(&mut Resources<'_, Uninterrupted>) -> R) -> R {
        let workers = Workers::new(1).unwrap();
        let memory = ComputeMemory::new(limit);
        f(&mut Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted })
    }

    #[test]
    fn shared_nodes_take_this_versions_dense_ids_and_the_rest_are_appended() {
        // This: 10 → 0, 20 → 1, 30 → 2, relation r0. Other: 5, 20, 30, 40
        // (dense 0..4), relations r1 then r0.
        let this = Build::first(&[10, 20, 30], &[(0, 10, 20, 1)], false, true);
        let other = Build::first(
            &[5, 20, 30, 40],
            &[(1, 5, 40, 1), (0, 20, 30, 1), (0, 40, 20, 1)],
            false,
            true,
        );
        with(1 << 20, |r| {
            let ids = align_ids(r, &this.version(), &other.version()).unwrap();
            assert_eq!(ids.dense_ids, [3, 1, 2, 4]);
            assert_eq!(ids.id_space, 5);
            assert_eq!(ids.relations.names(), ["r0", "r1"]);
        });
    }

    #[test]
    fn a_node_tombstoned_in_this_version_is_appended() {
        // This: 10 → 0, 20 → 1 (tombstoned), 30 → 2. Other: 20, 30, 40.
        let this =
            Build::first(&[10, 20, 30], &[], false, false).next(&[10, 30], &[], false, false);
        let other = Build::first(&[20, 30, 40], &[], false, false);
        with(1 << 20, |r| {
            let ids = align_ids(r, &this.version(), &other.version()).unwrap();
            assert_eq!((ids.dense_ids.as_slice(), ids.id_space), (&[3, 2, 4][..], 5));
        });
    }

    proptest! {
        /// Text versions align as the int versions of their ranks do.
        #[test]
        fn text_versions_align_as_int_versions_do(
            ranks in ranks(),
            history in builds(3),
        ) {
            let int: Vec<_> = history.iter().map(|(nodes, edges)| Build::first(nodes, edges, false, false)).collect();
            let text: Vec<_> = history
                .iter()
                .map(|(nodes, edges)| Build::first_with(&ranks, nodes, edges, false, false))
                .collect();
            let this = (int[0].next(&history[1].0, &[], false, false), text[0].next_with(&ranks, &history[1].0, &[], false, false));
            for (int_other, text_other) in int.iter().zip(&text) {
                with(1 << 20, |r| {
                    let expected = align_ids(r, &this.0.version(), &int_other.version()).unwrap();
                    let aligned = align_ids(r, &this.1.version(), &text_other.version()).unwrap();
                    assert_eq!(aligned.dense_ids, expected.dense_ids);
                    assert_eq!(aligned.id_space, expected.id_space);
                });
            }
        }
    }

    #[test]
    fn a_version_aligned_to_itself_is_unchanged() {
        let v = Build::first(&[1, 2, 3], &[(0, 1, 2, 3), (1, 3, 1, 2)], true, false);
        with(1 << 20, |r| {
            let ids = align_ids(r, &v.version(), &v.version()).unwrap();
            assert_eq!((ids.dense_ids.as_slice(), ids.id_space), (&[0, 1, 2][..], 3));
            assert_eq!(&ids.relations, &v.relations);
        });
    }

    /// The mapping is all an alignment charges: a dense id per node of
    /// the other version, held until the alignment is dropped.
    #[test]
    fn the_mapping_is_charged_to_the_budget() {
        let this = Build::first(&[1, 2, 3], &[(0, 1, 2, 3)], true, true);
        let other = Build::first(&[2, 3, 4, 5], &[(1, 5, 2, 1)], false, false);
        let workers = Workers::new(1).unwrap();
        let memory = ComputeMemory::new(1 << 20);
        let mut r =
            Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted };
        let ids = align_ids(&mut r, &this.version(), &other.version()).unwrap();
        assert_eq!(memory.in_use(), 4 * 8);
        assert_eq!(ids.dense_ids.capacity(), 4);
        drop(ids);
        assert_eq!(memory.in_use(), 0);
        with(4 * 8 - 1, |r| {
            assert!(matches!(
                align_ids(r, &this.version(), &other.version()),
                Err(SpectralError::Compute(ComputeError::MemoryLimit { .. }))
            ));
        });
    }
}
