//! Admission (DESIGN.md §5.6): whether a version of a given size may be
//! loaded into the shared cache, which slot it takes and which unpinned
//! versions are evicted, least recently used first, to make room under
//! `warren.shared_cache_memory`. A refusal is never an error: the caller
//! loads the version privately, as 0.4.0 does.

use super::slot::{Slot, State};
use crate::activity::Refused;
use crate::cache::lru::{evictions, Held};

/// What bounds the shared cache when a version is admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    /// `warren.shared_cache_memory`, in bytes: loading, ready and draining
    /// slots together. `0` disables the shared cache.
    pub(crate) cap: u64,
    /// The free bytes where a new region would be created (`/dev/shm` for
    /// `posix` segments).
    pub(crate) store_room: u64,
}

/// Why a version is loaded privately instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The cap is `0`.
    Disabled,
    /// The version is larger than the cap.
    TooLarge,
    /// The pinned, loading, draining and mapped versions leave no room for
    /// it.
    NoRoom,
    /// Every slot is in use and none can be freed now.
    NoSlot,
    /// Its region would not fit where regions are created.
    StoreFull,
    /// No region could be created: the `Dsm` adapter's segments take
    /// Postgres's DSM control slots, which were all in use
    /// (`DSM_CREATE_NULL_IF_MAXSEGMENTS`).
    NoSegment,
}

impl Refusal {
    /// The reason `warren.backends()` counts it under (DESIGN.md §5.7): a
    /// cap of `0` is the shared cache off, so unavailable.
    pub(crate) fn reason(self) -> Refused {
        match self {
            Self::Disabled => Refused::Unavailable,
            Self::TooLarge => Refused::TooLarge,
            Self::NoRoom => Refused::NoRoom,
            Self::NoSlot => Refused::NoSlot,
            Self::StoreFull => Refused::StoreFull,
            Self::NoSegment => Refused::NoSegment,
        }
    }
}

/// How a version is admitted.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// The slot it takes: empty now, or emptied by an eviction.
    pub(crate) slot: usize,
    /// The slots to evict first (reclaimable ones, so each is empty at
    /// once).
    pub(crate) evict: Vec<usize>,
}

/// How a version of `bytes` is admitted into `slots`, whose pins are
/// `pins`, under `limits`, or why it is not. Nothing waits for room: what
/// is pinned, loading, draining or mapped stays, and the least recently
/// used of the other ready slots are evicted until the version fits. A
/// mapped slot makes no room: evicted, it would drain, and it counts until
/// its mappers release it. The registry reads `pins` under its lock, and
/// only the lock's holder adds a pin, so a slot read unpinned stays
/// unpinned while the plan is carried out.
pub(crate) fn admit(
    slots: &[Slot],
    pins: &[u32],
    bytes: u64,
    limits: Limits,
) -> Result<Plan, Refusal> {
    if limits.cap == 0 {
        return Err(Refusal::Disabled);
    }
    if bytes > limits.cap {
        return Err(Refusal::TooLarge);
    }
    let staying = slots
        .iter()
        .zip(pins)
        .filter(|(slot, &pins)| slot.occupied() && !slot.reclaimable(pins))
        .map(|(slot, _)| slot.bytes)
        .fold(bytes, u64::saturating_add);
    if staying > limits.cap {
        return Err(Refusal::NoRoom);
    }
    let mut evict = to_fit(slots, pins, staying, limits.cap);
    let empty = slots.iter().position(|slot| slot.state == State::Empty);
    let slot = match empty.or_else(|| evict.first().copied()) {
        Some(slot) => slot,
        None => {
            let slot = least_recent_reclaimable(slots, pins).ok_or(Refusal::NoSlot)?;
            evict.push(slot);
            slot
        }
    };
    let room = evict
        .iter()
        .map(|&evicted| slots[evicted].bytes)
        .fold(limits.store_room, u64::saturating_add);
    if bytes > room {
        return Err(Refusal::StoreFull);
    }
    Ok(Plan { slot, evict })
}

/// The slots to drain after a refusal of a version of `bytes` over
/// `slots`, whose pins are `pins`, under `cap`, so that room appears for it
/// once their mappers release them (DESIGN.md §5.6, §14 item 28): a mapped
/// slot makes no room at once, but backends keep their mappings across
/// statements and release those of draining slots at their next message.
/// For `NoRoom`, the least recently used mapped unpinned ready slots whose
/// bytes, gone, would let the version fit, or none if all of them would
/// not; for `NoSlot`, the least recently used such slot; for any other
/// refusal, none.
pub(crate) fn to_drain(
    slots: &[Slot],
    pins: &[u32],
    bytes: u64,
    refusal: Refusal,
    cap: u64,
) -> Vec<usize> {
    let mut mapped: Vec<(usize, &Slot)> = slots
        .iter()
        .zip(pins)
        .enumerate()
        .filter(|(_, (slot, &pins))| slot.evictable(pins) && slot.mappers > 0)
        .map(|(index, (slot, _))| (index, slot))
        .collect();
    mapped.sort_by_key(|(_, slot)| slot.last_used);
    match refusal {
        Refusal::NoRoom => {
            let mut staying = slots
                .iter()
                .zip(pins)
                .filter(|(slot, &pins)| slot.occupied() && !slot.reclaimable(pins))
                .map(|(slot, _)| slot.bytes)
                .fold(bytes, u64::saturating_add);
            let chosen: Vec<usize> = mapped
                .into_iter()
                .take_while(|(_, slot)| {
                    let over = staying > cap;
                    staying -= slot.bytes;
                    over
                })
                .map(|(index, _)| index)
                .collect();
            if staying > cap {
                Vec::new()
            } else {
                chosen
            }
        }
        Refusal::NoSlot => mapped.first().map(|&(index, _)| vec![index]).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The reclaimable slots to evict, least recently used first, so that
/// they and the `staying` bytes fit in `cap` (`cache::lru`'s policy).
fn to_fit(slots: &[Slot], pins: &[u32], staying: u64, cap: u64) -> Vec<usize> {
    let held = reclaimable(slots, pins)
        .map(|(index, slot)| Held { key: index, bytes: slot.bytes, last_used: slot.last_used })
        .collect();
    let evicted = evictions(held, staying, cap);
    evicted.unfit.into_iter().map(|held| held.key).chain(evicted.least_recent).collect()
}

/// The least recently used reclaimable slot.
fn least_recent_reclaimable(slots: &[Slot], pins: &[u32]) -> Option<usize> {
    reclaimable(slots, pins).min_by_key(|(_, slot)| slot.last_used).map(|(index, _)| index)
}

/// The reclaimable slots and their indexes.
fn reclaimable<'a>(
    slots: &'a [Slot],
    pins: &'a [u32],
) -> impl Iterator<Item = (usize, &'a Slot)> + 'a {
    slots
        .iter()
        .zip(pins)
        .enumerate()
        .filter_map(|(index, (slot, &pins))| slot.reclaimable(pins).then_some((index, slot)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::shared::slot::Key;
    use pgrx::pg_sys::Oid;

    const ROOMY: u64 = u64::MAX;

    /// A free slot, unpinned.
    const EMPTY: (Slot, u32) = (Slot::EMPTY, 0);

    /// A slot in `state` holding `bytes`, last used at `tick`, with `pins`
    /// and `mappers`.
    fn slot(state: State, bytes: u64, tick: u64, pins: u32, mappers: u32) -> (Slot, u32) {
        let key = Key::version(Oid::from(1), Oid::from(2), tick as i64);
        (Slot { state, key, bytes, last_used: tick, mappers, ..Slot::EMPTY }, pins)
    }

    /// How a version of `bytes` is admitted into `table`'s slots and pins.
    fn admit(table: &[(Slot, u32)], bytes: u64, limits: Limits) -> Result<Plan, Refusal> {
        let (slots, pins): (Vec<Slot>, Vec<u32>) = table.iter().copied().unzip();
        super::admit(&slots, &pins, bytes, limits)
    }

    fn limits(cap: u64) -> Limits {
        Limits { cap, store_room: ROOMY }
    }

    #[test]
    fn a_version_that_fits_takes_the_first_empty_slot() {
        let slots = [slot(State::Ready, 30, 1, 0, 1), EMPTY, EMPTY];
        assert_eq!(admit(&slots, 70, limits(100)), Ok(Plan { slot: 1, evict: vec![] }));
    }

    #[test]
    fn a_cap_of_zero_disables_the_shared_cache() {
        assert_eq!(admit(&[EMPTY], 0, limits(0)), Err(Refusal::Disabled));
    }

    #[test]
    fn a_version_larger_than_the_cap_is_refused() {
        assert_eq!(admit(&[EMPTY], 101, limits(100)), Err(Refusal::TooLarge));
    }

    #[test]
    fn the_least_recently_used_unpinned_versions_are_evicted_to_make_room() {
        let slots = [
            slot(State::Ready, 30, 3, 0, 0),
            slot(State::Ready, 30, 1, 0, 0),
            slot(State::Ready, 30, 2, 0, 0),
            EMPTY,
        ];
        assert_eq!(admit(&slots, 40, limits(100)), Ok(Plan { slot: 3, evict: vec![1] }));
        assert_eq!(admit(&slots, 70, limits(100)), Ok(Plan { slot: 3, evict: vec![1, 2] }));
    }

    #[test]
    fn pinned_loading_and_draining_versions_are_never_evicted() {
        let slots = [
            slot(State::Ready, 30, 1, 1, 1),
            slot(State::Loading, 20, 0, 0, 0),
            slot(State::Draining, 20, 2, 0, 1),
            slot(State::Ready, 20, 3, 0, 0),
            EMPTY,
        ];
        assert_eq!(admit(&slots, 30, limits(100)), Ok(Plan { slot: 4, evict: vec![3] }));
        assert_eq!(admit(&slots, 31, limits(100)), Err(Refusal::NoRoom), "nothing waits for room");
    }

    #[test]
    fn a_mapped_version_makes_no_room() {
        let slots = [slot(State::Ready, 50, 1, 0, 1), EMPTY];
        assert_eq!(admit(&slots, 50, limits(100)), Ok(Plan { slot: 1, evict: vec![] }));
        assert_eq!(
            admit(&slots, 51, limits(100)),
            Err(Refusal::NoRoom),
            "evicted, it would drain, and a draining version counts until it is released"
        );
    }

    #[test]
    fn a_full_table_reuses_a_slot_its_eviction_empties() {
        let unmapped = [slot(State::Ready, 30, 2, 0, 0), slot(State::Ready, 30, 1, 0, 0)];
        assert_eq!(
            admit(&unmapped, 10, limits(100)),
            Ok(Plan { slot: 1, evict: vec![1] }),
            "the least recently used unmapped slot is evicted for its slot alone"
        );
        let overfull = [slot(State::Ready, 30, 2, 0, 1), slot(State::Ready, 30, 1, 0, 0)];
        assert_eq!(admit(&overfull, 70, limits(100)), Ok(Plan { slot: 1, evict: vec![1] }));
    }

    #[test]
    fn a_full_table_of_mapped_or_pinned_slots_is_refused() {
        let slots = [
            slot(State::Ready, 10, 1, 0, 1),
            slot(State::Ready, 10, 2, 1, 1),
            slot(State::Loading, 10, 0, 0, 0),
            slot(State::Draining, 10, 3, 0, 1),
        ];
        assert_eq!(admit(&slots, 10, limits(100)), Err(Refusal::NoSlot));
    }

    /// The slots `table` drains for a version of `bytes` refused under
    /// `limits`.
    fn drain(table: &[(Slot, u32)], bytes: u64, limits: Limits) -> Vec<usize> {
        let (slots, pins): (Vec<Slot>, Vec<u32>) = table.iter().copied().unzip();
        let refusal = super::admit(&slots, &pins, bytes, limits).expect_err("refused");
        super::to_drain(&slots, &pins, bytes, refusal, limits.cap)
    }

    #[test]
    fn a_refusal_for_room_drains_the_least_recently_used_mapped_versions_that_make_it() {
        let slots = [
            slot(State::Ready, 40, 3, 0, 1),
            slot(State::Ready, 40, 1, 0, 2),
            slot(State::Ready, 20, 2, 1, 1),
            slot(State::Ready, 30, 0, 0, 0),
        ];
        assert_eq!(admit(&slots, 40, limits(100)), Err(Refusal::NoRoom));
        assert_eq!(drain(&slots, 40, limits(100)), vec![1]);
        assert_eq!(drain(&slots, 80, limits(100)), vec![1, 0]);
        assert_eq!(
            drain(&slots, 81, limits(100)),
            Vec::<usize>::new(),
            "draining every mapped version would not make room beside the pinned one"
        );
    }

    #[test]
    fn a_full_table_drains_the_least_recently_used_mapped_slot() {
        let slots = [
            slot(State::Ready, 10, 2, 0, 1),
            slot(State::Ready, 10, 1, 0, 1),
            slot(State::Ready, 10, 3, 1, 1),
        ];
        assert_eq!(admit(&slots, 10, limits(100)), Err(Refusal::NoSlot));
        assert_eq!(drain(&slots, 10, limits(100)), vec![1]);
        let pinned = [slot(State::Ready, 10, 1, 1, 1), slot(State::Loading, 10, 0, 0, 0)];
        assert_eq!(drain(&pinned, 10, limits(100)), Vec::<usize>::new());
    }

    #[test]
    fn other_refusals_drain_nothing() {
        let slots = [slot(State::Ready, 30, 1, 0, 1), EMPTY];
        assert_eq!(drain(&slots, 10, limits(0)), Vec::<usize>::new(), "disabled");
        assert_eq!(drain(&slots, 101, limits(100)), Vec::<usize>::new(), "too large");
        let store = Limits { cap: 100, store_room: 10 };
        assert_eq!(drain(&slots, 20, store), Vec::<usize>::new(), "store full");
    }

    #[test]
    fn a_region_that_would_not_fit_in_the_store_is_refused() {
        let slots = [slot(State::Ready, 30, 1, 0, 0), slot(State::Ready, 30, 2, 0, 1), EMPTY];
        let store = |room| Limits { cap: 100, store_room: room };
        assert_eq!(admit(&slots, 40, store(39)), Err(Refusal::StoreFull));
        assert_eq!(admit(&slots, 40, store(40)), Ok(Plan { slot: 2, evict: vec![] }));
        assert_eq!(
            admit(&slots, 50, store(20)),
            Ok(Plan { slot: 2, evict: vec![0] }),
            "an evicted region nobody maps is freed at once"
        );
        assert_eq!(admit(&slots, 70, store(39)), Err(Refusal::StoreFull));
    }
}
