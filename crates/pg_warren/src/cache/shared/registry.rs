//! The registry's state machine (DESIGN.md §5.6): the transitions of its
//! slots, as plain Rust over a slot table, so that the `Dsm` adapter runs
//! them under its lock on a table in shared memory and the `Memory` fake
//! on one in a `Mutex`. Each slot's pins are an atomic counter in a pin
//! table beside it: they are added only under the lock, and a statement
//! ends them without it (`unpin`), so that the end of a transaction takes
//! no lock; admission and eviction read them under the lock, where a slot
//! read unpinned stays unpinned until the lock is released.
//!
//! ```text
//!            claim            publish            evict
//!   empty ---------> loading ---------> ready ---------> draining
//!     ^                 |                 |  <--revive--    |
//!     +---- abandon ----+                 |                 |
//!     +------ evict (no mapper) ----------+                 |
//!     +---------------- release (last mapper) --------------+
//! ```
//!
//! An evicted region is only unpinned: its memory is freed when its last
//! mapper detaches. Until then a lookup of its key finds it, and the
//! reader revives it (pinning its region again) rather than load the
//! version anew beside it, which could only drain another version. Every transition after a claim names the claim's
//! `Ticket`, so one on a slot since emptied and claimed again does nothing.

use std::sync::atomic::{AtomicU32, Ordering};

use super::admission::{admit, to_drain, Limits, Refusal};
use super::slot::{Handle, Key, Process, Slot, State, Ticket};

/// A table of slots: a `Vec` in the `Memory` fake, an array in shared
/// memory in the `Dsm` adapter.
pub(crate) trait SlotTable: AsRef<[Slot]> + AsMut<[Slot]> {}

impl<T: AsRef<[Slot]> + AsMut<[Slot]>> SlotTable for T {}

/// The slots' pins, one counter per slot, which a statement ends without
/// the registry's lock: shared with the backends in the `Memory` fake, an
/// array in shared memory beside the slots in the `Dsm` adapter.
pub(crate) trait PinTable: AsRef<[AtomicU32]> {}

impl<T: AsRef<[AtomicU32]>> PinTable for T {}

/// The slot table, its pins and its clocks.
#[derive(Debug)]
pub(crate) struct Registry<S, P> {
    clocks: Clocks,
    slots: S,
    pins: P,
}

/// A registry's clocks, kept in shared memory beside its slots by the
/// `Dsm` adapter, which resumes a registry over them under its lock.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Clocks {
    /// The logical clock that orders uses for eviction.
    clock: u64,
    /// The last claim's generation.
    generation: u64,
    /// How many slots have started draining: a backend that maps nothing
    /// evicted since it last looked need not take the lock to release.
    drains: u64,
}

impl Clocks {
    /// The clocks of a new registry.
    pub(crate) const NEW: Self = Self { clock: 0, generation: 0, drains: 0 };

    /// How many slots have started draining.
    pub(crate) fn drains(self) -> u64 {
        self.drains
    }
}

/// What the registry holds for a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lookup {
    /// Loaded: pin and attach it.
    Ready(Ticket),
    /// Being loaded: wait for it.
    Loading(Ticket),
    /// Evicted but still mapped: pin its region again, revive it, then
    /// read it as a ready one.
    Draining(Ticket),
    /// Neither: claim it.
    Missing,
}

/// The outcome of a claim.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Claim {
    /// Load the version into a new region, and publish or abandon it.
    /// `evicted` are the regions evicted to make room, to unpin.
    Load { ticket: Ticket, evicted: Vec<Handle> },
    /// Another backend has claimed it since the lookup: look again.
    Taken,
    /// Load it privately. `drained` are the mapped regions drained so that
    /// room appears once their mappers release them, to unpin.
    Refused { refusal: Refusal, drained: Vec<Handle> },
}

/// The slots in use, each with its pins, and the registry's clock: a
/// snapshot taken under its lock.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Listing {
    /// The loading, ready and draining slots, with their pins.
    pub(crate) slots: Vec<(Slot, u32)>,
    /// The clock that orders uses: a slot's `last_used` is its value when
    /// the slot was last pinned.
    pub(crate) clock: u64,
}

/// The slot no longer holds the use a ticket names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Gone;

/// Ends a statement's pin of the slot `ticket` names, without the
/// registry's lock: the pin keeps the slot ready, so the ticket still names
/// it, and an evicting backend that reads the count after this sees it.
pub(crate) fn unpin(pins: &[AtomicU32], ticket: Ticket) {
    if let Some(count) = pins.get(ticket.slot) {
        // A pin never ends twice; `None` (no pin) leaves the count at 0.
        let _ = count.fetch_update(Ordering::Release, Ordering::Relaxed, |n| n.checked_sub(1));
    }
}

impl<S: SlotTable, P: PinTable> Registry<S, P> {
    /// A registry over `slots`, all empty, and their `pins`, all 0.
    #[cfg(test)]
    pub(crate) fn new(slots: S, pins: P) -> Self {
        Self::resume(Clocks::NEW, slots, pins)
    }

    /// The registry whose state is `clocks`, `slots` and `pins`.
    pub(crate) fn resume(clocks: Clocks, slots: S, pins: P) -> Self {
        Self { clocks, slots, pins }
    }

    /// Its clocks now, to keep for `resume`.
    pub(crate) fn clocks(&self) -> Clocks {
        self.clocks
    }

    /// Its slots.
    pub(crate) fn slots(&self) -> &[Slot] {
        self.slots.as_ref()
    }

    /// Its slots' pin counters, which `unpin` decrements.
    pub(crate) fn pin_table(&self) -> &[AtomicU32] {
        self.pins.as_ref()
    }

    /// Its slots' pins now.
    pub(crate) fn pins(&self) -> Vec<u32> {
        self.pin_table().iter().map(|count| count.load(Ordering::Acquire)).collect()
    }

    /// What it holds for `key`: its loading or ready slot, or else its
    /// most recently claimed draining one.
    pub(crate) fn lookup(&self, key: Key) -> Lookup {
        let ticket =
            |index: usize, slot: &Slot| Ticket { slot: index, generation: slot.generation };
        let mut draining: Option<Ticket> = None;
        for (index, slot) in self.slots().iter().enumerate().filter(|(_, slot)| slot.key == key) {
            match slot.state {
                State::Ready => return Lookup::Ready(ticket(index, slot)),
                State::Loading => return Lookup::Loading(ticket(index, slot)),
                State::Draining if draining.is_none_or(|d| d.generation < slot.generation) => {
                    draining = Some(ticket(index, slot));
                }
                _ => {}
            }
        }
        draining.map_or(Lookup::Missing, Lookup::Draining)
    }

    /// Makes a draining slot ready again, once its reader has pinned its
    /// region again (it is still mapped, so still whole).
    pub(crate) fn revive(&mut self, ticket: Ticket) -> Result<(), Gone> {
        let tick = self.tick();
        let slot = self.held(ticket, State::Draining).ok_or(Gone)?;
        slot.state = State::Ready;
        slot.last_used = tick;
        Ok(())
    }

    /// Claims a slot for `loader` to load `key`'s version of `bytes` into,
    /// evicting what admission chooses. If it is refused, only the mapped
    /// versions `admission::to_drain` chooses are evicted, to drain.
    pub(crate) fn claim(&mut self, key: Key, bytes: u64, loader: Process, limits: Limits) -> Claim {
        if self.lookup(key) != Lookup::Missing {
            return Claim::Taken;
        }
        let pins = self.pins();
        let plan = match admit(self.slots(), &pins, bytes, limits) {
            Ok(plan) => plan,
            Err(refusal) => {
                let chosen = to_drain(self.slots(), &pins, bytes, refusal, limits.cap);
                let drained = chosen.into_iter().map(|index| self.retire(index)).collect();
                return Claim::Refused { refusal, drained };
            }
        };
        let evicted = plan.evict.into_iter().map(|index| self.retire(index)).collect();
        self.clocks.generation += 1;
        let generation = self.clocks.generation;
        self.slots.as_mut()[plan.slot] =
            Slot { state: State::Loading, key, bytes, loader, generation, ..Slot::EMPTY };
        Claim::Load { ticket: Ticket { slot: plan.slot, generation }, evicted }
    }

    /// Publishes a loaded region as `handle`: the slot is ready, pinned by
    /// the loader's statement and mapped by the loader.
    pub(crate) fn publish(&mut self, ticket: Ticket, handle: Handle) -> Result<(), Gone> {
        let tick = self.tick();
        let slot = self.held(ticket, State::Loading).ok_or(Gone)?;
        slot.state = State::Ready;
        slot.handle = handle;
        slot.mappers = 1;
        slot.last_used = tick;
        self.pin_table()[ticket.slot].store(1, Ordering::Relaxed);
        Ok(())
    }

    /// Empties a loading slot whose loader failed or is gone, so that a
    /// waiter takes over. Whether it did.
    pub(crate) fn abandon(&mut self, ticket: Ticket) -> bool {
        self.held(ticket, State::Loading).map(|slot| slot.state = State::Empty).is_some()
    }

    /// Pins a ready slot for a statement and marks it used.
    pub(crate) fn pin(&mut self, ticket: Ticket) -> Result<(), Gone> {
        let tick = self.tick();
        let slot = self.held(ticket, State::Ready).ok_or(Gone)?;
        slot.last_used = tick;
        self.pin_table()[ticket.slot].fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// The handle of a ready slot's region, to attach.
    pub(crate) fn handle(&self, ticket: Ticket) -> Result<Handle, Gone> {
        match self.state(ticket) {
            Some(State::Ready) => Ok(self.slots()[ticket.slot].handle),
            _ => Err(Gone),
        }
    }

    /// Counts a new mapper of a ready slot, once its region is attached.
    pub(crate) fn attach(&mut self, ticket: Ticket) -> Result<(), Gone> {
        let slot = self.held(ticket, State::Ready).ok_or(Gone)?;
        slot.mappers += 1;
        Ok(())
    }

    /// Ends a mapping: a draining slot is empty after its last.
    pub(crate) fn release(&mut self, ticket: Ticket) {
        let Some(slot) = self.current(ticket) else {
            return;
        };
        slot.mappers = slot.mappers.saturating_sub(1);
        if slot.state == State::Draining && slot.mappers == 0 {
            slot.state = State::Empty;
        }
    }

    /// Evicts the unpinned ready slots whose keys `which` selects; the
    /// regions to unpin.
    pub(crate) fn evict(&mut self, which: impl Fn(&Key) -> bool) -> Vec<Handle> {
        let pins = self.pins();
        let chosen: Vec<usize> = self
            .slots()
            .iter()
            .zip(pins)
            .enumerate()
            .filter(|(_, (slot, pins))| slot.evictable(*pins) && which(&slot.key))
            .map(|(index, _)| index)
            .collect();
        chosen.into_iter().map(|index| self.retire(index)).collect()
    }

    /// The keys of the occupied slots.
    pub(crate) fn keys(&self) -> Vec<Key> {
        self.slots().iter().filter(|slot| slot.occupied()).map(|slot| slot.key).collect()
    }

    /// Every slot in use (loading, ready or draining) with its pins, and
    /// the clock now: what `warren.shared_cache()` shows (DESIGN.md §14
    /// item 37).
    pub(crate) fn listing(&self) -> Listing {
        let slots = self.slots().iter().copied().zip(self.pins());
        Listing {
            slots: slots.filter(|(slot, _)| slot.occupied()).collect(),
            clock: self.clocks.clock,
        }
    }

    /// The state of the use `ticket` names, if the slot still holds it.
    pub(crate) fn state(&self, ticket: Ticket) -> Option<State> {
        let slot = self.slots().get(ticket.slot)?;
        (slot.generation == ticket.generation && slot.occupied()).then_some(slot.state)
    }

    /// The backend loading the use `ticket` names, while it loads.
    pub(crate) fn loader(&self, ticket: Ticket) -> Option<Process> {
        let loading = self.state(ticket) == Some(State::Loading);
        loading.then(|| self.slots()[ticket.slot].loader)
    }

    /// Evicts the ready slot at `index`: draining until its mappers
    /// release it, or empty if it has none. Its region, to unpin.
    fn retire(&mut self, index: usize) -> Handle {
        let slot = &mut self.slots.as_mut()[index];
        slot.state = if slot.mappers == 0 { State::Empty } else { State::Draining };
        let handle = slot.handle;
        if slot.state == State::Draining {
            self.clocks.drains += 1;
        }
        handle
    }

    /// The slot holding the use `ticket` names, if it does.
    fn current(&mut self, ticket: Ticket) -> Option<&mut Slot> {
        self.state(ticket)?;
        self.slots.as_mut().get_mut(ticket.slot)
    }

    /// The slot holding the use `ticket` names, if it is in `state`.
    fn held(&mut self, ticket: Ticket, state: State) -> Option<&mut Slot> {
        self.current(ticket).filter(|slot| slot.state == state)
    }

    /// The next tick of the clock.
    fn tick(&mut self) -> u64 {
        self.clocks.clock += 1;
        self.clocks.clock
    }
}

#[cfg(test)]
mod tests {
    use pgrx::pg_sys::Oid;

    use super::*;
    use crate::cache::shared::slot::Kind;

    const LOADER: Process = Process { pid: 100, started: 1 };

    fn key(version: i64) -> Key {
        Key::version(Oid::from(1), Oid::from(2), version)
    }

    fn limits(cap: u64) -> Limits {
        Limits { cap, store_room: u64::MAX }
    }

    type Table = Registry<Vec<Slot>, Vec<AtomicU32>>;

    fn registry(slots: usize) -> Table {
        Registry::new(vec![Slot::EMPTY; slots], (0..slots).map(|_| AtomicU32::new(0)).collect())
    }

    /// Claims `version` of `bytes` under a cap of 100; its ticket.
    fn claimed(registry: &mut Table, version: i64, bytes: u64) -> Ticket {
        match registry.claim(key(version), bytes, LOADER, limits(100)) {
            Claim::Load { ticket, evicted } => {
                assert!(evicted.is_empty());
                ticket
            }
            other => panic!("not claimed: {other:?}"),
        }
    }

    /// Loads and publishes `version` as handle `version`, then ends the
    /// loader's pin, keeping its mapping.
    fn loaded(registry: &mut Table, version: i64, bytes: u64) -> Ticket {
        let ticket = claimed(registry, version, bytes);
        registry.publish(ticket, Handle(version as u32)).unwrap();
        unpin(registry.pin_table(), ticket);
        ticket
    }

    #[test]
    fn a_miss_is_claimed_once_and_loads() {
        let mut registry = registry(2);
        assert_eq!(registry.lookup(key(1)), Lookup::Missing);
        let ticket = claimed(&mut registry, 1, 10);
        assert_eq!(registry.lookup(key(1)), Lookup::Loading(ticket));
        assert_eq!(registry.loader(ticket), Some(LOADER));
        assert_eq!(
            registry.claim(key(1), 10, Process { pid: 101, ..LOADER }, limits(100)),
            Claim::Taken
        );
        assert_eq!(registry.state(ticket), Some(State::Loading));
        assert_eq!(registry.slots()[ticket.slot].bytes, 10);
    }

    #[test]
    fn a_version_and_a_vector_of_the_same_id_are_held_apart() {
        let mut registry = registry(2);
        let version = loaded(&mut registry, 1, 10);
        let vector = Key::vector(Oid::from(1), Oid::from(2), 1);
        assert_eq!(registry.lookup(vector), Lookup::Missing);
        let Claim::Load { ticket, .. } = registry.claim(vector, 10, LOADER, limits(100)) else {
            panic!("not claimed");
        };
        assert_ne!(ticket.slot, version.slot);
        assert_eq!(registry.lookup(key(1)), Lookup::Ready(version));
        assert_eq!(registry.evict(|key| key.kind == Kind::Vector), Vec::<Handle>::new());
        assert_eq!(registry.lookup(vector), Lookup::Loading(ticket), "a loading slot stays");
    }

    #[test]
    fn a_registry_resumed_from_its_clocks_continues_them() {
        let mut first = registry(2);
        let ticket = loaded(&mut first, 1, 10);
        let (clocks, slots) = (first.clocks(), first.slots().to_vec());
        let used = slots[ticket.slot].last_used;
        let pins: Vec<AtomicU32> = first.pins().into_iter().map(AtomicU32::new).collect();
        let mut resumed = Registry::resume(clocks, slots, pins);
        assert_eq!(resumed.lookup(key(1)), Lookup::Ready(ticket));
        let again = claimed(&mut resumed, 2, 10);
        assert_eq!(again.generation, ticket.generation + 1);
        resumed.pin(ticket).unwrap();
        assert!(resumed.slots()[ticket.slot].last_used > used, "its clock goes on");
    }

    #[test]
    fn a_published_version_is_ready_pinned_and_mapped_by_its_loader() {
        let mut registry = registry(2);
        let ticket = claimed(&mut registry, 1, 10);
        assert_eq!(registry.publish(ticket, Handle(7)), Ok(()));
        assert_eq!(registry.lookup(key(1)), Lookup::Ready(ticket));
        assert_eq!(registry.loader(ticket), None);
        let slot = registry.slots()[ticket.slot];
        assert_eq!(
            (slot.state, slot.handle, registry.pins()[ticket.slot], slot.mappers),
            (State::Ready, Handle(7), 1, 1)
        );
        assert_eq!(registry.claim(key(1), 10, LOADER, limits(100)), Claim::Taken);
    }

    #[test]
    fn an_abandoned_claim_empties_its_slot_and_its_ticket_goes_stale() {
        let mut registry = registry(1);
        let ticket = claimed(&mut registry, 1, 10);
        assert!(registry.abandon(ticket));
        assert!(!registry.abandon(ticket), "only once");
        assert_eq!(registry.lookup(key(1)), Lookup::Missing);
        let again = claimed(&mut registry, 1, 10);
        assert_eq!(again.slot, ticket.slot);
        assert_ne!(again, ticket, "a new claim has a new generation");
        assert_eq!(registry.publish(ticket, Handle(1)), Err(Gone), "the old loader cannot publish");
        assert!(!registry.abandon(ticket), "nor abandon the new claim");
        assert_eq!(registry.state(again), Some(State::Loading));
    }

    #[test]
    fn readers_pin_and_attach_a_ready_slot() {
        let mut registry = registry(1);
        let ticket = loaded(&mut registry, 1, 10);
        assert_eq!(registry.handle(ticket), Ok(Handle(1)));
        assert_eq!(registry.slots()[ticket.slot].mappers, 1, "not counted before it attaches");
        assert_eq!(registry.attach(ticket), Ok(()));
        assert_eq!(registry.pin(ticket), Ok(()));
        assert_eq!(registry.pin(ticket), Ok(()));
        let slot = registry.slots()[ticket.slot];
        assert_eq!((registry.pins()[ticket.slot], slot.mappers), (2, 2));
        unpin(registry.pin_table(), ticket);
        assert_eq!(registry.pins()[ticket.slot], 1);
    }

    #[test]
    fn a_loading_or_stale_slot_cannot_be_pinned_or_attached() {
        let mut registry = registry(1);
        let ticket = claimed(&mut registry, 1, 10);
        assert_eq!(registry.pin(ticket), Err(Gone));
        assert_eq!(registry.handle(ticket), Err(Gone));
        assert_eq!(registry.attach(ticket), Err(Gone));
        registry.abandon(ticket);
        let again = loaded(&mut registry, 2, 10);
        assert_eq!(again.slot, ticket.slot);
        assert_eq!(registry.pin(ticket), Err(Gone));
        assert_eq!(registry.handle(ticket), Err(Gone));
        assert_eq!(registry.attach(ticket), Err(Gone));
        assert_eq!(registry.state(ticket), None);
    }

    #[test]
    fn eviction_spares_pinned_slots_and_drains_mapped_ones() {
        let mut registry = registry(3);
        let pinned = loaded(&mut registry, 1, 10);
        registry.pin(pinned).unwrap();
        let mapped = loaded(&mut registry, 2, 10);
        let unmapped = loaded(&mut registry, 3, 10);
        registry.release(unmapped);
        assert_eq!(registry.evict(|_| true), vec![Handle(2), Handle(3)]);
        assert_eq!(registry.state(pinned), Some(State::Ready));
        assert_eq!(registry.state(mapped), Some(State::Draining));
        assert_eq!(registry.state(unmapped), None, "nobody maps it: empty at once");
        assert_eq!(registry.lookup(key(2)), Lookup::Draining(mapped), "found, to revive");
        assert_eq!(registry.pin(mapped), Err(Gone));
        assert_eq!(registry.handle(mapped), Err(Gone));
        assert_eq!(registry.attach(mapped), Err(Gone));
        assert_eq!(registry.evict(|_| true), vec![], "evicted once");
        registry.release(mapped);
        assert_eq!(registry.state(mapped), None, "its last mapper empties it");
    }

    #[test]
    fn the_drain_count_counts_slots_that_start_draining() {
        let mut registry = registry(3);
        let mapped = loaded(&mut registry, 1, 10);
        let unmapped = loaded(&mut registry, 2, 10);
        registry.release(unmapped);
        assert_eq!(registry.clocks().drains(), 0);
        registry.evict(|key| key.id == 2);
        assert_eq!(registry.clocks().drains(), 0, "emptied at once, not drained");
        registry.evict(|key| key.id == 1);
        assert_eq!(registry.clocks().drains(), 1);
        registry.release(mapped);
        assert_eq!(registry.clocks().drains(), 1, "its end is not counted");
    }

    #[test]
    fn eviction_selects_by_key() {
        let mut registry = registry(2);
        let first = loaded(&mut registry, 1, 10);
        let second = loaded(&mut registry, 2, 10);
        assert_eq!(registry.evict(|key| key.id == 2), vec![Handle(2)]);
        assert_eq!(registry.state(first), Some(State::Ready));
        assert_eq!(registry.state(second), Some(State::Draining));
    }

    #[test]
    fn a_claim_evicts_the_least_recently_pinned_to_make_room() {
        let mut registry = registry(3);
        let first = loaded(&mut registry, 1, 40);
        let second = loaded(&mut registry, 2, 40);
        registry.release(first);
        registry.release(second);
        registry.pin(first).unwrap();
        unpin(registry.pin_table(), first);
        let Claim::Load { ticket, evicted } = registry.claim(key(3), 40, LOADER, limits(100))
        else {
            panic!("not admitted");
        };
        assert_eq!(evicted, vec![Handle(2)]);
        assert_eq!(registry.state(second), None, "nobody mapped it");
        assert_eq!(registry.state(first), Some(State::Ready));
        assert_eq!(ticket.slot, 2, "an empty slot is taken first");
    }

    #[test]
    fn a_claim_takes_the_slot_its_eviction_empties() {
        let mut registry = registry(1);
        let old = loaded(&mut registry, 1, 10);
        registry.release(old);
        let Claim::Load { ticket, evicted } = registry.claim(key(2), 10, LOADER, limits(100))
        else {
            panic!("not admitted");
        };
        assert_eq!((ticket.slot, evicted), (old.slot, vec![Handle(1)]));
        assert_eq!(registry.lookup(key(2)), Lookup::Loading(ticket));
    }

    #[test]
    fn a_claim_refused_for_room_drains_mapped_versions_for_the_next() {
        let mut registry = registry(3);
        let older = loaded(&mut registry, 1, 40);
        let newer = loaded(&mut registry, 2, 40);
        assert_eq!(
            registry.claim(key(3), 40, LOADER, limits(100)),
            Claim::Refused { refusal: Refusal::NoRoom, drained: vec![Handle(1)] }
        );
        assert_eq!(registry.state(older), Some(State::Draining));
        assert_eq!(registry.state(newer), Some(State::Ready));
        registry.release(older);
        assert!(
            matches!(registry.claim(key(3), 40, LOADER, limits(100)), Claim::Load { .. }),
            "admitted once the drained version's mapper releases it"
        );
    }

    #[test]
    fn a_draining_version_found_again_is_revived() {
        let mut registry = registry(3);
        let older = loaded(&mut registry, 1, 40);
        let newer = loaded(&mut registry, 2, 40);
        let refused = registry.claim(key(3), 40, LOADER, limits(100));
        assert!(matches!(refused, Claim::Refused { refusal: Refusal::NoRoom, .. }));
        assert_eq!(registry.lookup(key(1)), Lookup::Draining(older));
        assert_eq!(
            registry.claim(key(1), 40, LOADER, limits(100)),
            Claim::Taken,
            "a claim finds it too, and looks again"
        );
        assert_eq!(registry.revive(older), Ok(()));
        assert_eq!(registry.lookup(key(1)), Lookup::Ready(older));
        assert_eq!(registry.state(newer), Some(State::Ready), "nothing else drained");
        assert_eq!(registry.revive(older), Err(Gone), "only a draining slot revives");
        registry.release(older);
        assert_eq!(registry.state(older), Some(State::Ready), "ready with no mapper");
    }

    #[test]
    fn a_refused_claim_that_cannot_drain_changes_nothing() {
        let mut registry = registry(2);
        let held = loaded(&mut registry, 1, 60);
        let before = registry.slots().to_vec();
        let refused = |refusal| Claim::Refused { refusal, drained: vec![] };
        assert_eq!(registry.claim(key(2), 50, LOADER, limits(0)), refused(Refusal::Disabled));
        assert_eq!(registry.claim(key(2), 101, LOADER, limits(100)), refused(Refusal::TooLarge));
        registry.pin(held).unwrap();
        assert_eq!(registry.claim(key(2), 50, LOADER, limits(100)), refused(Refusal::NoRoom));
        unpin(registry.pin_table(), held);
        assert_eq!(registry.pins()[0], 0);
        assert_eq!(registry.slots()[0].state, before[0].state);
        assert_eq!(registry.slots()[1], before[1]);
    }
}
