//! The registry's state machine (DESIGN.md §5.6): the transitions of its
//! slots, as plain Rust over a slot table, so that the `Dsm` adapter runs
//! them under its lock on a table in shared memory and the `Memory` fake
//! on one in a `Mutex`.
//!
//! ```text
//!            claim            publish
//!   empty ---------> loading ---------> ready --evict--> draining
//!     ^                 |                 |                 |
//!     +---- abandon ----+                 |                 |
//!     +------ evict (no mapper) ----------+                 |
//!     +---------------- release (last mapper) --------------+
//! ```
//!
//! An evicted region is only unpinned: its memory is freed when its last
//! mapper detaches. Every transition after a claim names the claim's
//! `Ticket`, so one on a slot since emptied and claimed again does nothing.

use super::admission::{admit, Limits, Refusal};
use super::slot::{Handle, Key, Process, Slot, State, Ticket};

/// A table of slots: a `Vec` in the `Memory` fake, an array in shared
/// memory in the `Dsm` adapter.
pub(crate) trait SlotTable: AsRef<[Slot]> + AsMut<[Slot]> {}

impl<T: AsRef<[Slot]> + AsMut<[Slot]>> SlotTable for T {}

/// The slot table and its clocks.
#[derive(Debug)]
pub(crate) struct Registry<S> {
    /// The logical clock that orders uses for eviction.
    clock: u64,
    /// The last claim's generation.
    generation: u64,
    slots: S,
}

/// What the registry holds for a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lookup {
    /// Loaded: pin and attach it.
    Ready(Ticket),
    /// Being loaded: wait for it.
    Loading(Ticket),
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
    /// Load it privately.
    Refused(Refusal),
}

/// The slot no longer holds the use a ticket names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Gone;

impl<S: SlotTable> Registry<S> {
    /// A registry over `slots`, all empty.
    pub(crate) fn new(slots: S) -> Self {
        Self { clock: 0, generation: 0, slots }
    }

    /// Its slots.
    pub(crate) fn slots(&self) -> &[Slot] {
        self.slots.as_ref()
    }

    /// What it holds for `key`.
    pub(crate) fn lookup(&self, key: Key) -> Lookup {
        let found = self.slots().iter().enumerate().find(|(_, slot)| {
            slot.key == key && matches!(slot.state, State::Loading | State::Ready)
        });
        match found {
            Some((index, slot)) => {
                let ticket = Ticket { slot: index, generation: slot.generation };
                if slot.state == State::Ready {
                    Lookup::Ready(ticket)
                } else {
                    Lookup::Loading(ticket)
                }
            }
            None => Lookup::Missing,
        }
    }

    /// Claims a slot for `loader` to load `key`'s version of `bytes` into,
    /// evicting what admission chooses; nothing changes if it is refused.
    pub(crate) fn claim(&mut self, key: Key, bytes: u64, loader: Process, limits: Limits) -> Claim {
        if self.lookup(key) != Lookup::Missing {
            return Claim::Taken;
        }
        let plan = match admit(self.slots(), bytes, limits) {
            Ok(plan) => plan,
            Err(refusal) => return Claim::Refused(refusal),
        };
        let evicted = plan.evict.into_iter().map(|index| self.retire(index)).collect();
        self.generation += 1;
        let generation = self.generation;
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
        slot.pins = 1;
        slot.mappers = 1;
        slot.last_used = tick;
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
        slot.pins += 1;
        slot.last_used = tick;
        Ok(())
    }

    /// Ends a statement's pin.
    pub(crate) fn unpin(&mut self, ticket: Ticket) {
        if let Some(slot) = self.held(ticket, State::Ready) {
            slot.pins = slot.pins.saturating_sub(1);
        }
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
        let chosen: Vec<usize> = self
            .slots()
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.evictable() && which(&slot.key))
            .map(|(index, _)| index)
            .collect();
        chosen.into_iter().map(|index| self.retire(index)).collect()
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
        slot.handle
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
        self.clock += 1;
        self.clock
    }
}

#[cfg(test)]
mod tests {
    use pgrx::pg_sys::Oid;

    use super::*;

    const LOADER: Process = Process { pid: 100, started: 1 };

    fn key(version: i64) -> Key {
        Key { database: Oid::from(1), extension: Oid::from(2), version }
    }

    fn limits(cap: u64) -> Limits {
        Limits { cap, store_room: u64::MAX }
    }

    fn registry(slots: usize) -> Registry<Vec<Slot>> {
        Registry::new(vec![Slot::EMPTY; slots])
    }

    /// Claims `version` of `bytes` under a cap of 100; its ticket.
    fn claimed(registry: &mut Registry<Vec<Slot>>, version: i64, bytes: u64) -> Ticket {
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
    fn loaded(registry: &mut Registry<Vec<Slot>>, version: i64, bytes: u64) -> Ticket {
        let ticket = claimed(registry, version, bytes);
        registry.publish(ticket, Handle(version as u32)).unwrap();
        registry.unpin(ticket);
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
    fn a_published_version_is_ready_pinned_and_mapped_by_its_loader() {
        let mut registry = registry(2);
        let ticket = claimed(&mut registry, 1, 10);
        assert_eq!(registry.publish(ticket, Handle(7)), Ok(()));
        assert_eq!(registry.lookup(key(1)), Lookup::Ready(ticket));
        assert_eq!(registry.loader(ticket), None);
        let slot = registry.slots()[ticket.slot];
        assert_eq!(
            (slot.state, slot.handle, slot.pins, slot.mappers),
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
        assert_eq!((slot.pins, slot.mappers), (2, 2));
        registry.unpin(ticket);
        assert_eq!(registry.slots()[ticket.slot].pins, 1);
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
        assert_eq!(registry.lookup(key(2)), Lookup::Missing, "a draining version is not found");
        assert_eq!(registry.pin(mapped), Err(Gone));
        assert_eq!(registry.handle(mapped), Err(Gone));
        assert_eq!(registry.attach(mapped), Err(Gone));
        assert_eq!(registry.evict(|_| true), vec![], "evicted once");
        registry.release(mapped);
        assert_eq!(registry.state(mapped), None, "its last mapper empties it");
    }

    #[test]
    fn eviction_selects_by_key() {
        let mut registry = registry(2);
        let first = loaded(&mut registry, 1, 10);
        let second = loaded(&mut registry, 2, 10);
        assert_eq!(registry.evict(|key| key.version == 2), vec![Handle(2)]);
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
        registry.unpin(first);
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
    fn a_refused_claim_changes_nothing() {
        let mut registry = registry(2);
        let held = loaded(&mut registry, 1, 60);
        let before = registry.slots().to_vec();
        assert_eq!(
            registry.claim(key(2), 50, LOADER, limits(0)),
            Claim::Refused(Refusal::Disabled)
        );
        assert_eq!(
            registry.claim(key(2), 101, LOADER, limits(100)),
            Claim::Refused(Refusal::TooLarge)
        );
        registry.pin(held).unwrap();
        assert_eq!(
            registry.claim(key(2), 50, LOADER, limits(100)),
            Claim::Refused(Refusal::NoRoom)
        );
        registry.unpin(held);
        assert_eq!(registry.slots()[0].pins, 0);
        assert_eq!(registry.slots()[0].state, before[0].state);
        assert_eq!(registry.slots()[1], before[1]);
    }
}
