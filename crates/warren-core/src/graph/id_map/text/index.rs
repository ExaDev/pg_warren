//! The hash index a build maps edge endpoints through (DESIGN.md §5.7): an
//! open-addressing table of dense ids with 2 slots a live node, indexed by
//! a multiplicative range reduction of an id's hash under std's keyed
//! default hasher (`RandomState`: secret keys, drawn per thread and varied
//! per instance; std does not promise which algorithm), probed linearly.
//! The keys are never persisted, and every hit is confirmed by comparing
//! bytes, so a collision changes no answer, only a probe's length. It is
//! `Sync`, so the compute pool can probe it.

use std::hash::{BuildHasher, RandomState};

use super::TextKeys;
use crate::graph::build_memory::bytes_of;
use crate::graph::{
    memory, BuildCharge, BuildMemory, DenseId, GraphError, IdKeys, IdMap, Target, TargetWidth,
};

/// A hash index over a text id map's live ids, answering as
/// [`IdMap::dense_id`] does.
#[derive(Debug)]
pub struct TextIndex<'m> {
    id_map: &'m IdMap<TextKeys>,
    slots: Slots,
    hasher: RandomState,
    _charge: BuildCharge,
}

/// The compute pool probes one index from many threads.
const _: fn() = || {
    fn sync<T: Sync>() {}
    sync::<TextIndex<'static>>();
};

/// Each slot holds a dense id plus 1, or 0 when empty, at the width of the
/// id space.
#[derive(Debug)]
enum Slots {
    U32(Vec<u32>),
    U64(Vec<u64>),
}

impl<'m> TextIndex<'m> {
    /// The index of `id_map`'s live ids, charged to `memory` for as long as
    /// it lives.
    pub fn new(id_map: &'m IdMap<TextKeys>, memory: &BuildMemory) -> Result<Self, GraphError> {
        let len = usize::try_from(id_map.node_count().saturating_mul(2).max(1))
            .map_err(|_| GraphError::IdSpaceTooLarge)?;
        let hasher = RandomState::new();
        let (slots, charge) = match TargetWidth::for_id_space(id_map.id_space()) {
            TargetWidth::U32 => {
                let (slots, charge) = filled(id_map, &hasher, len, memory)?;
                (Slots::U32(slots), charge)
            }
            TargetWidth::U64 => {
                let (slots, charge) = filled(id_map, &hasher, len, memory)?;
                (Slots::U64(slots), charge)
            }
        };
        Ok(Self { id_map, slots, hasher, _charge: charge })
    }

    /// The dense id of a live node, or `None` if `id` is no live node's.
    pub fn dense_id(&self, id: &[u8]) -> Option<DenseId> {
        let hash = self.hasher.hash_one(id);
        match &self.slots {
            Slots::U32(slots) => find(self.id_map.keys(), slots, hash, id),
            Slots::U64(slots) => find(self.id_map.keys(), slots, hash, id),
        }
    }

    /// The heap bytes it holds.
    pub fn bytes(&self) -> u64 {
        match &self.slots {
            Slots::U32(slots) => memory::held(slots),
            Slots::U64(slots) => memory::held(slots),
        }
    }
}

/// The first slot `hash` probes among `len`.
fn home(hash: u64, len: usize) -> usize {
    ((u128::from(hash) * len as u128) >> 64) as usize
}

/// `len` slots holding every live node of `id_map`, charged to `memory`.
fn filled<T: Target>(
    id_map: &IdMap<TextKeys>,
    hasher: &RandomState,
    len: usize,
    memory: &BuildMemory,
) -> Result<(Vec<T>, BuildCharge), GraphError> {
    let charge = memory.charge(bytes_of::<T>(len))?;
    let mut slots = memory::filled(len, T::default())?;
    let live = (0..id_map.id_space()).filter(|&dense| !id_map.tombstones().contains(dense));
    for dense in live {
        let id = id_map.keys().key(dense).ok_or(GraphError::IdMapNotPermutation)?;
        let stored = T::from_dense(dense + 1).ok_or(GraphError::IdSpaceExceedsTargetWidth)?;
        let mut slot = home(hasher.hash_one(id), len);
        while slots[slot] != T::default() {
            slot = (slot + 1) % len;
        }
        slots[slot] = stored;
    }
    Ok((slots, charge))
}

/// The dense id in `slots` whose id is `id`, probing from `hash`'s home
/// slot to the first empty one.
fn find<T: Target>(keys: &TextKeys, slots: &[T], hash: u64, id: &[u8]) -> Option<DenseId> {
    let mut slot = home(hash, slots.len());
    loop {
        let stored = slots.get(slot)?.dense();
        let dense = stored.checked_sub(1)?;
        if keys.key(dense) == Some(id) {
            return Some(dense);
        }
        slot = (slot + 1) % slots.len();
    }
}
