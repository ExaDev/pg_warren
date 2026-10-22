//! A backend's side of the shared cache: the slots its current statement
//! pins, the regions it maps (each an `Arc`, so a mapping a call still
//! holds is never released from under it), and the slots it is loading.
//! Its operations run under the registry's lock, except `unpin`, which
//! needs only the registry's pin table, and keep the registry's pins and
//! `mappers` equal to what the backends hold, so that a backend's exit
//! gives back everything it held.
//!
//! When unheld mappings are released is the adapter's policy (DESIGN.md
//! §14 item 28): every unheld mapping (`Release::Unheld`), or only those of
//! evicted slots (`Release::Evicted`).

use std::sync::atomic::AtomicU32;
use std::sync::Arc;

use super::admission::Limits;
use super::registry::{unpin, Claim, Gone, PinTable, Registry, SlotTable};
use super::slot::{Handle, Key, Process, State, Ticket};

/// A backend's pins, mappings and claims; `M` is a mapped region.
#[derive(Debug)]
pub(crate) struct Session<M> {
    process: Process,
    /// The slots the current statement pins.
    pins: Vec<Ticket>,
    /// The regions this backend maps.
    mappings: Vec<(Ticket, Arc<M>)>,
    /// The slots this backend claimed and is loading.
    loading: Vec<Ticket>,
}

/// How to read a ready slot.
#[derive(Debug)]
pub(crate) enum Use<M> {
    /// Through this backend's mapping.
    Mapped(Arc<M>),
    /// Attach this region, then record it with `Session::mapped`, which
    /// counts this backend a mapper: an attach that fails counts nothing.
    Attach(Handle),
}

/// A mapping of a slot the registry no longer has ready: the caller
/// detaches it.
#[derive(Debug)]
pub(crate) struct Unattached<M>(pub(crate) Arc<M>);

/// A backend's mapping of a published region.
pub(crate) trait Mapped {
    /// Whether a call holds what was read through it apart from its `Arc`
    /// (a loaded version built over it, which the mapping keeps): such a
    /// mapping is held, and never released.
    fn in_use(&self) -> bool;
}

/// Which mappings to release. Neither releases a mapping a call holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Release {
    /// Those of slots evicted (draining) since they were mapped.
    Evicted,
    /// Every one.
    // DESIGN.md §14 item 28's other answer, pending the owner: `dsm::DETACH`
    // switches to it in one line.
    #[cfg_attr(not(test), allow(dead_code))]
    Unheld,
}

impl<M> Session<M> {
    /// The session of the backend `process`, which holds nothing.
    pub(crate) fn new(process: Process) -> Self {
        Self { process, pins: Vec::new(), mappings: Vec::new(), loading: Vec::new() }
    }

    /// The backend's process.
    #[cfg(test)]
    pub(crate) fn process(&self) -> Process {
        self.process
    }

    /// Uses a ready slot in the current statement, pinning it once per
    /// statement.
    pub(crate) fn open<S: SlotTable, P: PinTable>(
        &mut self,
        registry: &mut Registry<S, P>,
        ticket: Ticket,
    ) -> Result<Use<M>, Gone> {
        if registry.state(ticket) != Some(State::Ready) {
            return Err(Gone);
        }
        if !self.pins.contains(&ticket) {
            registry.pin(ticket)?;
            self.pins.push(ticket);
        }
        match self.mappings.iter().find(|(mapped, _)| *mapped == ticket) {
            Some((_, mapping)) => Ok(Use::Mapped(Arc::clone(mapping))),
            None => registry.handle(ticket).map(Use::Attach),
        }
    }

    /// Records the mapping of a slot `open` said to attach, counting this
    /// backend a mapper. The statement's pin keeps the slot ready, so the
    /// registry refuses it only if the slot was not opened: then the
    /// mapping comes back, to detach.
    pub(crate) fn mapped<S: SlotTable, P: PinTable>(
        &mut self,
        registry: &mut Registry<S, P>,
        ticket: Ticket,
        mapping: Arc<M>,
    ) -> Result<Arc<M>, Unattached<M>> {
        match registry.attach(ticket) {
            Ok(()) => Ok(self.held(ticket, mapping)),
            Err(Gone) => Err(Unattached(mapping)),
        }
    }

    /// Records a mapping the registry already counts.
    fn held(&mut self, ticket: Ticket, mapping: Arc<M>) -> Arc<M> {
        self.mappings.push((ticket, Arc::clone(&mapping)));
        mapping
    }

    /// Claims a slot to load `key`'s version of `bytes` into.
    pub(crate) fn claim<S: SlotTable, P: PinTable>(
        &mut self,
        registry: &mut Registry<S, P>,
        key: Key,
        bytes: u64,
        limits: Limits,
    ) -> Claim {
        let claim = registry.claim(key, bytes, self.process, limits);
        if let Claim::Load { ticket, .. } = claim {
            self.loading.push(ticket);
        }
        claim
    }

    /// Publishes a claimed slot's region, which this backend maps as
    /// `mapping`; the statement pins it.
    pub(crate) fn publish<S: SlotTable, P: PinTable>(
        &mut self,
        registry: &mut Registry<S, P>,
        ticket: Ticket,
        handle: Handle,
        mapping: Arc<M>,
    ) -> Result<Arc<M>, Gone> {
        self.loading.retain(|&loading| loading != ticket);
        registry.publish(ticket, handle)?;
        self.pins.push(ticket);
        Ok(self.held(ticket, mapping))
    }

    /// Gives up every slot the backend is loading, as the abort callback
    /// does after an ERROR unwound the load, so that a waiter loads it. Its
    /// mappings stay, and its pins end with the statement as usual; an
    /// attach the ERROR interrupted was never counted (`Session::mapped`).
    pub(crate) fn abort<S: SlotTable, P: PinTable>(&mut self, registry: &mut Registry<S, P>) {
        for ticket in self.loading.drain(..) {
            registry.abandon(ticket);
        }
    }

    /// Ends the current statement's pins, without the registry's lock.
    pub(crate) fn unpin(&mut self, pins: &[AtomicU32]) {
        for ticket in self.pins.drain(..) {
            unpin(pins, ticket);
        }
    }

    /// Releases the mappings `which` selects that no call holds, through
    /// their `Arc` or what was read through them, one at a time (see
    /// `release_one`).
    pub(crate) fn release<S: SlotTable, P: PinTable>(
        &mut self,
        registry: &mut Registry<S, P>,
        which: Release,
        mut detach: impl FnMut(Arc<M>),
    ) where
        M: Mapped,
    {
        let mut index = 0;
        while let Some((ticket, mapping)) = self.mappings.get(index) {
            let selected = match which {
                Release::Evicted => registry.state(*ticket) != Some(State::Ready),
                Release::Unheld => true,
            };
            if selected && Arc::strong_count(mapping) == 1 && !mapping.in_use() {
                self.release_one(registry, index, &mut detach);
            } else {
                index += 1;
            }
        }
    }

    /// Gives back everything the backend holds as it exits: its pins, its
    /// claims and every mapping, held or not.
    pub(crate) fn exit<S: SlotTable, P: PinTable>(
        &mut self,
        registry: &mut Registry<S, P>,
        mut detach: impl FnMut(Arc<M>),
    ) {
        self.unpin(registry.pin_table());
        self.abort(registry);
        while !self.mappings.is_empty() {
            self.release_one(registry, 0, &mut detach);
        }
    }

    /// Releases the mapping at `index`: the registry counts it gone, then
    /// `detach` detaches it. One at a time, so that an ERROR in a detach
    /// leaves every other mapping both held here and counted there; the
    /// one it interrupts is counted gone already (a mapper counted forever
    /// would keep its slot draining), and at worst stays mapped until the
    /// backend exits.
    fn release_one<S: SlotTable, P: PinTable>(
        &mut self,
        registry: &mut Registry<S, P>,
        index: usize,
        detach: &mut impl FnMut(Arc<M>),
    ) {
        let (ticket, mapping) = self.mappings.swap_remove(index);
        registry.release(ticket);
        detach(mapping);
    }
}

impl<M> Session<M> {
    /// The slots the current statement pins.
    #[cfg(test)]
    pub(crate) fn pinned(&self) -> &[Ticket] {
        &self.pins
    }

    /// The slots this backend maps.
    pub(crate) fn mapped_slots(&self) -> impl Iterator<Item = Ticket> + '_ {
        self.mappings.iter().map(|(ticket, _)| *ticket)
    }

    /// The slots this backend is loading.
    pub(crate) fn loading(&self) -> &[Ticket] {
        &self.loading
    }
}

#[cfg(test)]
mod tests {
    use pgrx::pg_sys::Oid;

    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::cache::shared::registry::Lookup;
    use crate::cache::shared::slot::Slot;

    const PROCESS: Process = Process { pid: 7, started: 1 };

    impl Mapped for u32 {
        fn in_use(&self) -> bool {
            false
        }
    }
    const OTHER: Process = Process { pid: 1, started: 2 };

    fn key(version: i64) -> Key {
        Key { database: Oid::from(1), extension: Oid::from(2), version }
    }

    fn limits() -> Limits {
        Limits { cap: 100, store_room: u64::MAX }
    }

    type Table = Registry<Vec<Slot>, Vec<AtomicU32>>;

    fn table(slots: usize) -> Table {
        Registry::new(vec![Slot::EMPTY; slots], (0..slots).map(|_| AtomicU32::new(0)).collect())
    }

    /// A registry holding `version`, loaded and published by another
    /// backend whose pin has ended (it still maps it), as handle `version`.
    fn published(versions: &[i64]) -> (Table, Vec<Ticket>) {
        let mut registry = table(4);
        let tickets = versions
            .iter()
            .map(|&version| {
                let Claim::Load { ticket, .. } = registry.claim(key(version), 10, OTHER, limits())
                else {
                    panic!("not claimed");
                };
                registry.publish(ticket, Handle(version as u32)).unwrap();
                unpin(registry.pin_table(), ticket);
                ticket
            })
            .collect();
        (registry, tickets)
    }

    /// Opens `ticket` in `session`, attaching it if needed; the mapping.
    fn read(session: &mut Session<u32>, registry: &mut Table, ticket: Ticket) -> Arc<u32> {
        match session.open(registry, ticket).unwrap() {
            Use::Mapped(mapping) => mapping,
            Use::Attach(Handle(handle)) => {
                session.mapped(registry, ticket, Arc::new(handle)).unwrap()
            }
        }
    }

    fn counts(registry: &Table, ticket: Ticket) -> (u32, u32) {
        (registry.pins()[ticket.slot], registry.slots()[ticket.slot].mappers)
    }

    #[test]
    fn a_statement_pins_and_attaches_a_slot_once() {
        let (mut registry, tickets) = published(&[1]);
        let mut session = Session::new(PROCESS);
        assert!(matches!(session.open(&mut registry, tickets[0]), Ok(Use::Attach(Handle(1)))));
        let mapping = session.mapped(&mut registry, tickets[0], Arc::new(1)).unwrap();
        let again = read(&mut session, &mut registry, tickets[0]);
        assert!(Arc::ptr_eq(&mapping, &again), "the mapping is reused");
        assert_eq!(counts(&registry, tickets[0]), (1, 2));
    }

    #[test]
    fn a_mapper_is_counted_only_once_its_attach_succeeds() {
        let (mut registry, tickets) = published(&[1]);
        let mut session = Session::<u32>::new(PROCESS);
        assert!(matches!(session.open(&mut registry, tickets[0]), Ok(Use::Attach(Handle(1)))));
        assert_eq!(counts(&registry, tickets[0]), (1, 1), "an attach that fails counts nothing");
        session.unpin(registry.pin_table());
        session.exit(&mut registry, |_| panic!("nothing mapped"));
        assert_eq!(counts(&registry, tickets[0]), (0, 1));
    }

    #[test]
    fn the_next_statement_pins_again_through_the_same_mapping() {
        let (mut registry, tickets) = published(&[1]);
        let mut session = Session::new(PROCESS);
        drop(read(&mut session, &mut registry, tickets[0]));
        session.unpin(registry.pin_table());
        assert_eq!(counts(&registry, tickets[0]), (0, 2));
        assert!(matches!(session.open(&mut registry, tickets[0]), Ok(Use::Mapped(_))));
        assert_eq!(counts(&registry, tickets[0]), (1, 2));
    }

    #[test]
    fn a_slot_no_longer_ready_cannot_be_opened() {
        let (mut registry, tickets) = published(&[1]);
        registry.evict(|_| true);
        let mut session = Session::<u32>::new(PROCESS);
        assert!(session.open(&mut registry, tickets[0]).is_err());
        session.unpin(registry.pin_table());
        session.release(&mut registry, Release::Unheld, |_| panic!("nothing mapped"));
        assert_eq!(counts(&registry, tickets[0]), (0, 1), "nothing was counted");
    }

    #[test]
    fn unheld_releases_every_mapping_no_call_holds() {
        let (mut registry, tickets) = published(&[1, 2]);
        let mut session = Session::new(PROCESS);
        let held = read(&mut session, &mut registry, tickets[0]);
        drop(read(&mut session, &mut registry, tickets[1]));
        session.unpin(registry.pin_table());
        let mut detached = Vec::new();
        session.release(&mut registry, Release::Unheld, |mapping| detached.push(*mapping));
        assert_eq!(detached, [2]);
        assert_eq!(counts(&registry, tickets[0]), (0, 2), "a held mapping stays");
        assert_eq!(counts(&registry, tickets[1]), (0, 1));
        drop(held);
        session.release(&mut registry, Release::Unheld, |mapping| detached.push(*mapping));
        assert_eq!(detached, [2, 1]);
    }

    /// A mapping that calls may hold apart from its `Arc`.
    #[derive(Debug)]
    struct Reading(AtomicBool);

    impl Mapped for Reading {
        fn in_use(&self) -> bool {
            self.0.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn a_mapping_in_use_apart_from_its_arc_is_not_released() {
        let (mut registry, tickets) = published(&[1]);
        let mut session = Session::new(PROCESS);
        let Ok(Use::Attach(_)) = session.open(&mut registry, tickets[0]) else {
            panic!("not attached");
        };
        let reading = Arc::new(Reading(AtomicBool::new(true)));
        drop(session.mapped(&mut registry, tickets[0], Arc::clone(&reading)).unwrap());
        drop(reading);
        session.unpin(registry.pin_table());
        session.release(&mut registry, Release::Unheld, |_| panic!("in use"));
        assert_eq!(counts(&registry, tickets[0]), (0, 2));
        let Some(Use::Mapped(mapping)) = session.open(&mut registry, tickets[0]).ok() else {
            panic!("not mapped");
        };
        mapping.0.store(false, Ordering::Relaxed);
        drop(mapping);
        session.unpin(registry.pin_table());
        let mut released = 0;
        session.release(&mut registry, Release::Unheld, |_| released += 1);
        assert_eq!((released, counts(&registry, tickets[0])), (1, (0, 1)));
    }

    #[test]
    fn evicted_releases_only_mappings_of_draining_slots() {
        let (mut registry, tickets) = published(&[1, 2]);
        let mut session = Session::new(PROCESS);
        drop(read(&mut session, &mut registry, tickets[0]));
        drop(read(&mut session, &mut registry, tickets[1]));
        session.unpin(registry.pin_table());
        registry.release(tickets[1]);
        registry.evict(|key| key.version == 2);
        assert_eq!(registry.state(tickets[1]), Some(State::Draining));
        let mut detached = Vec::new();
        session.release(&mut registry, Release::Evicted, |mapping| detached.push(*mapping));
        assert_eq!(detached, [2]);
        assert_eq!(registry.state(tickets[1]), None, "its last mapper emptied it");
        assert_eq!(counts(&registry, tickets[0]), (0, 2), "a ready slot stays mapped");
    }

    #[test]
    fn an_error_in_a_detach_leaves_the_other_mappings_counted() {
        let (mut registry, tickets) = published(&[1, 2, 3]);
        let mut session = Session::new(PROCESS);
        for &ticket in &tickets {
            drop(read(&mut session, &mut registry, ticket));
            registry.release(ticket);
        }
        session.unpin(registry.pin_table());
        registry.evict(|_| true);
        let mut detaches = 0;
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            session.release(&mut registry, Release::Evicted, |_| {
                detaches += 1;
                assert!(detaches < 2, "an ERROR in dsm_detach");
            });
        }));
        assert!(failed.is_err());
        let mapped: Vec<Ticket> = session.mapped_slots().collect();
        assert_eq!(mapped.len(), 1, "the mapping not yet released is still held");
        for &ticket in &tickets {
            let mappers = registry.slots()[ticket.slot].mappers;
            assert_eq!(mappers, u32::from(mapped.contains(&ticket)), "{ticket:?}");
        }
    }

    #[test]
    fn a_mapping_the_registry_does_not_count_is_given_back_to_detach() {
        let (mut registry, tickets) = published(&[1]);
        let mut session = Session::new(PROCESS);
        registry.release(tickets[0]);
        registry.evict(|_| true);
        let mapping = Arc::new(1);
        let refused = session.mapped(&mut registry, tickets[0], Arc::clone(&mapping));
        assert!(matches!(refused, Err(Unattached(given)) if Arc::ptr_eq(&given, &mapping)));
        assert_eq!(session.mapped_slots().count(), 0);
    }

    #[test]
    fn a_published_load_is_pinned_and_mapped_by_its_loader() {
        let mut registry = table(2);
        let mut session = Session::new(PROCESS);
        let Claim::Load { ticket, .. } = session.claim(&mut registry, key(1), 10, limits()) else {
            panic!("not claimed");
        };
        assert_eq!(registry.loader(ticket), Some(PROCESS));
        let mapping = session.publish(&mut registry, ticket, Handle(9), Arc::new(9)).unwrap();
        assert_eq!(counts(&registry, ticket), (1, 1));
        let again = read(&mut session, &mut registry, ticket);
        assert!(Arc::ptr_eq(&mapping, &again));
        assert_eq!(counts(&registry, ticket), (1, 1), "pinned once in the statement");
    }

    #[test]
    fn an_abort_gives_up_its_loads_and_keeps_its_pins_and_mappings() {
        let (mut registry, tickets) = published(&[1]);
        let mut session = Session::new(PROCESS);
        let held = read(&mut session, &mut registry, tickets[0]);
        let Claim::Load { ticket, .. } = session.claim(&mut registry, key(2), 10, limits()) else {
            panic!("not claimed");
        };
        session.abort(&mut registry);
        assert_eq!(registry.lookup(key(2)), Lookup::Missing, "a waiter loads it");
        assert_eq!(registry.state(ticket), None);
        assert!(session.loading().is_empty());
        assert_eq!(counts(&registry, tickets[0]), (1, 2), "the statement's end unpins it");
        assert_eq!(session.mapped_slots().collect::<Vec<_>>(), [tickets[0]]);
        drop(held);
    }

    #[test]
    fn an_exit_gives_back_its_pins_claims_and_mappings() {
        let (mut registry, tickets) = published(&[1, 2]);
        let mut session = Session::new(PROCESS);
        let held = read(&mut session, &mut registry, tickets[0]);
        drop(read(&mut session, &mut registry, tickets[1]));
        let Claim::Load { ticket: loading, .. } =
            session.claim(&mut registry, key(3), 10, limits())
        else {
            panic!("not claimed");
        };
        registry.release(tickets[1]);
        let mut detached = Vec::new();
        session.exit(&mut registry, |mapping| detached.push(*mapping));
        detached.sort_unstable();
        assert_eq!(detached, [1, 2], "held or not");
        assert_eq!(counts(&registry, tickets[0]), (0, 1));
        assert_eq!(counts(&registry, tickets[1]), (0, 0));
        assert_eq!(registry.state(loading), None, "its claim is given up");
        drop(held);
    }
}
