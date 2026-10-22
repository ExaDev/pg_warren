//! The shared cache against a reference model (DESIGN.md §5.6): random
//! interleavings of what several backends do (read, wait for a load and
//! take it over, finish a load, raise an ERROR in a load or an attach, end
//! a statement, drop what calls hold, release mappings (or one version's), evict, exit, and
//! vanish without the exit callback) on a `Memory` cluster, checking after
//! each step that the cap holds, that no pinned version is evicted, that
//! each version has at most one loading or ready slot, that every loading
//! slot has a loader (live, or vanished until a waiter takes it over),
//! that the registry's pins and mappers are what the backends hold, that
//! a read of a draining version revives it rather than load it again, and
//! that refusals are those of a simple model of admission. At the end of
//! each case, once every live backend has released what it holds, every
//! slot that no vanished backend holds is reclaimable and no draining slot
//! is left; and once every backend has exited, every region is freed.

use std::sync::Arc;
use std::time::Duration;

use pgrx::pg_sys::Oid;
use proptest::prelude::*;

use super::admission::Refusal;
use super::memory::{Backend, Loading, Mapping, Memory};
use super::session::Session;
use super::slot::{Key, Process, Slot, State, Ticket};
use super::{Claimed, Found, SharedVersions};

const BACKENDS: usize = 3;
/// The versions and vectors read: two of each, alternating (`key`).
const ENTRIES: usize = 4;

/// What one backend does next.
#[derive(Debug, Clone)]
enum Op {
    /// Reads a version: finds it, notes the load it would wait for, or
    /// claims it.
    Read(usize, usize),
    /// Waits for the load its last read found, taking it over if its
    /// loader is gone, then reads again.
    Wait(usize),
    /// Publishes the version it is loading.
    Publish(usize),
    /// Raises an ERROR, which drops the region it is loading, if any, and
    /// runs the abort callback.
    Fail(usize),
    /// Reads a version whose attach raises an ERROR, then runs the abort
    /// callback.
    FailAttach(usize, usize),
    /// Ends its statement.
    EndStatement(usize),
    /// Its calls stop holding what they read.
    DropViews(usize),
    /// Releases its mappings of evicted versions.
    Release(usize),
    /// Evicts a version.
    Evict(usize, usize),
    /// Releases its mapping of a version, ready or not (`warren.evict`).
    Forget(usize, usize),
    /// Exits (FATAL); a new backend takes its place.
    Exit(usize),
    /// Vanishes without its exit callback; a new backend takes its place.
    Vanish(usize),
}

fn op() -> impl Strategy<Value = Op> {
    let backend = 0..BACKENDS;
    let version = 0..ENTRIES;
    prop_oneof![
        5 => (backend.clone(), version.clone()).prop_map(|(b, v)| Op::Read(b, v)),
        2 => backend.clone().prop_map(Op::Wait),
        3 => backend.clone().prop_map(Op::Publish),
        1 => backend.clone().prop_map(Op::Fail),
        1 => (backend.clone(), version.clone()).prop_map(|(b, v)| Op::FailAttach(b, v)),
        3 => backend.clone().prop_map(Op::EndStatement),
        2 => backend.clone().prop_map(Op::DropViews),
        3 => backend.clone().prop_map(Op::Release),
        1 => (backend.clone(), version.clone()).prop_map(|(b, v)| Op::Evict(b, v)),
        1 => (backend.clone(), version).prop_map(|(b, v)| Op::Forget(b, v)),
        1 => backend.clone().prop_map(Op::Exit),
        1 => backend.prop_map(Op::Vanish),
    ]
}

/// The key of entry `entry`: versions and vectors alternate, so a version
/// and a vector share each id (DESIGN.md §14 item 38).
fn key(entry: usize) -> Key {
    let id = (entry / 2) as i64;
    match entry % 2 {
        0 => Key::version(Oid::from(1), Oid::from(2), id),
        _ => Key::vector(Oid::from(1), Oid::from(2), id),
    }
}

/// A backend and what its calls hold.
struct Simulated {
    port: Backend,
    /// The regions its calls read, by version.
    views: Vec<(usize, Arc<Mapping>)>,
    /// The version it is loading.
    pending: Option<(usize, Loading)>,
    /// The version its last read found loading, and that load.
    waiting: Option<(usize, Ticket)>,
}

impl Simulated {
    fn new(memory: &Memory) -> Self {
        Self { port: memory.backend(), views: Vec::new(), pending: None, waiting: None }
    }

    fn session(&self) -> &Session<Mapping> {
        self.port.session()
    }
}

/// The cluster's configuration.
struct Config {
    cap: u64,
    capacity: u64,
    sizes: Vec<u64>,
}

/// A cluster and its backends: those alive, and those that vanished.
struct World {
    memory: Memory,
    config: Config,
    backends: Vec<Simulated>,
    vanished: Vec<Simulated>,
}

/// The registry's slots and their pins now.
fn table(memory: &Memory) -> Vec<(Slot, u32)> {
    memory.slots().into_iter().zip(memory.pins()).collect()
}

/// What admission should decide for a version of `bytes` over `slots` and
/// their pins: `None` if it may be admitted (unless the store is full).
fn model_refusal(slots: &[(Slot, u32)], bytes: u64, cap: u64) -> Option<Refusal> {
    let reclaimable =
        |(slot, pins): &(Slot, u32)| slot.state == State::Ready && *pins == 0 && slot.mappers == 0;
    let staying: u64 = slots
        .iter()
        .filter(|entry| entry.0.state != State::Empty && !reclaimable(entry))
        .map(|(slot, _)| slot.bytes)
        .sum::<u64>()
        + bytes;
    let free = slots.iter().any(|entry| entry.0.state == State::Empty || reclaimable(entry));
    if cap == 0 {
        Some(Refusal::Disabled)
    } else if bytes > cap {
        Some(Refusal::TooLarge)
    } else if staying > cap {
        Some(Refusal::NoRoom)
    } else if !free {
        Some(Refusal::NoSlot)
    } else {
        None
    }
}

/// What a claim refused with `refusal` changed, from `before` to `after`:
/// only mapped, unpinned ready slots drained, and only for `NoRoom` (enough
/// that the version of `bytes` fits once they are released) or `NoSlot`
/// (one slot).
fn drained_as_modelled(
    before: &[(Slot, u32)],
    after: &[(Slot, u32)],
    bytes: u64,
    refusal: Refusal,
    cap: u64,
) -> Result<(), TestCaseError> {
    let mut drained = 0;
    for ((was, pins), (now, _)) in before.iter().zip(after) {
        if was == now {
            continue;
        }
        prop_assert!(was.state == State::Ready && *pins == 0 && was.mappers > 0);
        prop_assert_eq!(now.state, State::Draining);
        drained += 1;
    }
    match refusal {
        Refusal::NoRoom if drained > 0 => {
            let staying: u64 = after
                .iter()
                .filter(|(slot, pins)| {
                    let reclaimable = slot.state == State::Ready && *pins == 0 && slot.mappers == 0;
                    matches!(slot.state, State::Loading | State::Ready) && !reclaimable
                })
                .map(|(slot, _)| slot.bytes)
                .sum();
            prop_assert!(staying + bytes <= cap, "draining left no room");
        }
        Refusal::NoRoom => {}
        Refusal::NoSlot => prop_assert!(drained <= 1),
        _ => prop_assert_eq!(drained, 0),
    }
    Ok(())
}

/// `backend` reads `version`.
fn read(
    memory: &Memory,
    config: &Config,
    backend: &mut Simulated,
    version: usize,
) -> Result<(), TestCaseError> {
    if backend.pending.is_some() {
        return Ok(());
    }
    backend.waiting = None;
    let before = table(memory);
    let allocated = memory.allocated();
    let of_version = |slot: &Slot| slot.key == key(version);
    let draining = before.iter().any(|(slot, _)| of_version(slot) && slot.state == State::Draining);
    let live = before
        .iter()
        .any(|(slot, _)| of_version(slot) && matches!(slot.state, State::Loading | State::Ready));
    let found = backend.port.lookup(key(version));
    if draining && !live {
        prop_assert!(matches!(found, Found::Ready(_)), "a draining version is revived");
        prop_assert_eq!(memory.allocated(), allocated, "and not loaded again");
    }
    match found {
        Found::Ready(view) => backend.views.push((version, view)),
        Found::Loading(loading) => backend.waiting = Some((version, loading)),
        Found::Missing => {
            let bytes = config.sizes[version];
            let expected = model_refusal(&before, bytes, config.cap);
            match backend.port.claim(key(version), bytes) {
                Claimed::Load(mut region) => {
                    prop_assert_eq!(expected, None);
                    region.bytes_mut().fill(version as u8 + 1);
                    backend.pending = Some((version, region));
                }
                Claimed::Taken => prop_assert!(false, "no other backend runs in between"),
                Claimed::Refused(Refusal::StoreFull) => {
                    prop_assert_eq!(expected, None);
                    prop_assert!(bytes > config.capacity.saturating_sub(allocated));
                }
                Claimed::Refused(refusal) => {
                    prop_assert_eq!(expected, Some(refusal));
                    drained_as_modelled(&before, &table(memory), bytes, refusal, config.cap)?;
                }
            }
        }
    }
    Ok(())
}

impl World {
    /// The sessions of every backend, alive or vanished.
    fn sessions(&self) -> impl Iterator<Item = &Session<Mapping>> {
        self.backends.iter().chain(&self.vanished).map(Simulated::session)
    }

    /// Whether `process` vanished.
    fn is_vanished(&self, process: Process) -> bool {
        self.vanished.iter().any(|backend| backend.session().process() == process)
    }

    /// Whether a vanished backend pins or maps the slot at `index`.
    fn held_by_vanished(&self, index: usize) -> bool {
        self.vanished.iter().any(|backend| {
            let session = backend.session();
            session.pinned().into_iter().any(|ticket| ticket.slot == index)
                || session.mapped_slots().any(|ticket| ticket.slot == index)
        })
    }

    /// Applies `op`.
    fn apply(&mut self, op: Op) -> Result<(), TestCaseError> {
        match op {
            Op::Read(b, version) => {
                read(&self.memory, &self.config, &mut self.backends[b], version)?;
            }
            Op::Wait(b) => self.wait(b)?,
            Op::Publish(b) => {
                let backend = &mut self.backends[b];
                if let Some((version, region)) = backend.pending.take() {
                    let view = backend.port.publish(region);
                    prop_assert!(view.is_ok(), "nothing takes a live loader's slot");
                    if let Ok(view) = view {
                        backend.views.push((version, view));
                    }
                }
            }
            Op::Fail(b) => {
                let backend = &mut self.backends[b];
                backend.pending = None;
                backend.port.abort();
            }
            Op::FailAttach(b, version) => {
                let backend = &mut self.backends[b];
                if backend.pending.is_none() {
                    backend.port.fail_attach(key(version));
                    backend.port.abort();
                }
            }
            Op::EndStatement(b) => self.backends[b].port.unpin(),
            Op::DropViews(b) => self.backends[b].views.clear(),
            Op::Release(b) => self.backends[b].port.release(),
            Op::Evict(b, version) => {
                self.backends[b].port.evict(&|evicted| *evicted == key(version));
            }
            Op::Forget(b, version) => {
                self.backends[b].port.forget(&|forgotten| *forgotten == key(version));
            }
            Op::Exit(b) => {
                let fresh = Simulated::new(&self.memory);
                std::mem::replace(&mut self.backends[b], fresh).port.exit();
            }
            Op::Vanish(b) => {
                let fresh = Simulated::new(&self.memory);
                let mut gone = std::mem::replace(&mut self.backends[b], fresh);
                gone.pending = None;
                gone.port.vanish();
                self.vanished.push(gone);
            }
        }
        Ok(())
    }

    /// Backend `b` waits for the load its last read found, taking over one
    /// whose loader vanished, then reads again.
    fn wait(&mut self, b: usize) -> Result<(), TestCaseError> {
        let Some((version, loading)) = self.backends[b].waiting.take() else {
            return Ok(());
        };
        let still =
            |slot: &Slot| slot.state == State::Loading && slot.generation == loading.generation;
        let slot = self.memory.slots()[loading.slot];
        let lost = still(&slot) && self.is_vanished(slot.loader);
        self.backends[b].port.wait(loading);
        let after = still(&self.memory.slots()[loading.slot]);
        prop_assert_eq!(
            after,
            still(&slot) && !lost,
            "only a vanished loader's load is taken over"
        );
        read(&self.memory, &self.config, &mut self.backends[b], version)
    }

    /// The invariants, after a step from `before`.
    fn check(&self, before: &[(Slot, u32)]) -> Result<(), TestCaseError> {
        let config = &self.config;
        let slots = self.memory.slots();
        let pinned = self.memory.pins();
        let occupied: u64 =
            slots.iter().filter(|slot| slot.state != State::Empty).map(|slot| slot.bytes).sum();
        prop_assert!(occupied <= config.cap, "{} bytes over a cap of {}", occupied, config.cap);
        for (index, ((was, pins), now)) in before.iter().zip(&slots).enumerate() {
            let still_ready = now.state == State::Ready && now.generation == was.generation;
            if was.state == State::Ready && !still_ready {
                prop_assert_eq!(*pins, 0, "slot {} was evicted while pinned", index);
            }
        }
        for version in 0..ENTRIES {
            let live = slots
                .iter()
                .filter(|slot| {
                    slot.key == key(version) && matches!(slot.state, State::Loading | State::Ready)
                })
                .count();
            prop_assert!(live <= 1, "version {} has {} live slots", version, live);
        }
        for (index, slot) in slots.iter().enumerate() {
            if slot.state == State::Empty {
                continue;
            }
            let ticket = Ticket { slot: index, generation: slot.generation };
            let pins = self.sessions().filter(|session| session.pinned().contains(&ticket)).count();
            let mappers = self
                .sessions()
                .filter(|session| session.mapped_slots().any(|mapped| mapped == ticket))
                .count();
            prop_assert_eq!(pinned[index] as usize, pins, "pins of slot {}", index);
            prop_assert_eq!(slot.mappers as usize, mappers, "mappers of slot {}", index);
            if slot.state == State::Draining {
                prop_assert!(slot.mappers > 0, "slot {} drains with no mapper", index);
            }
            if slot.state == State::Loading {
                let loaders: Vec<_> =
                    self.sessions().filter(|session| session.loading().contains(&ticket)).collect();
                prop_assert_eq!(loaders.len(), 1, "slot {} has no loader", index);
                prop_assert_eq!(loaders[0].process(), slot.loader);
            }
        }
        for backend in self.backends.iter().chain(&self.vanished) {
            for ticket in backend.session().pinned() {
                prop_assert_eq!(slots[ticket.slot].state, State::Ready);
            }
            for (version, view) in &backend.views {
                let bytes = config.sizes[*version] as usize;
                prop_assert_eq!(view.bytes(), &vec![*version as u8 + 1; bytes][..]);
            }
        }
        Ok(())
    }

    /// Every live backend ends what it does and releases its evicted
    /// mappings: then no slot drains. Evicted then and released again,
    /// every slot is empty: every ready slot was reclaimable. Slots that
    /// vanished backends hold are the exceptions.
    fn settle(&mut self) -> Result<(), TestCaseError> {
        for backend in &mut self.backends {
            backend.pending = None;
            backend.port.abort();
            backend.views.clear();
            backend.port.unpin();
            backend.port.release();
        }
        for (index, slot) in self.memory.slots().iter().enumerate() {
            if slot.state == State::Draining {
                prop_assert!(self.held_by_vanished(index), "slot {} still drains", index);
            }
        }
        let settled = table(&self.memory);
        self.check(&settled)?;
        if let Some(backend) = self.backends.first_mut() {
            backend.port.evict(&|_| true);
        }
        for backend in &mut self.backends {
            backend.port.release();
        }
        let settled = table(&self.memory);
        self.check(&settled)?;
        for (index, (slot, _)) in settled.into_iter().enumerate() {
            if self.held_by_vanished(index) {
                continue;
            }
            match slot.state {
                State::Ready => prop_assert!(false, "slot {} is not reclaimable", index),
                State::Loading => prop_assert!(self.is_vanished(slot.loader)),
                State::Draining => prop_assert!(false, "slot {} still drains", index),
                State::Empty => {}
            }
        }
        Ok(())
    }
}

proptest! {
    #[test]
    fn the_shared_cache_keeps_its_invariants(
        slots in 1usize..6,
        cap in prop_oneof![1 => Just(0u64), 9 => 20u64..200],
        capacity in prop_oneof![3 => Just(1000u64), 1 => 0u64..300],
        sizes in prop::collection::vec(1u64..70, ENTRIES),
        ops in prop::collection::vec(op(), 1..80),
    ) {
        let memory = Memory::new(slots, cap, capacity, Duration::ZERO);
        let config = Config { cap, capacity, sizes };
        let backends = (0..BACKENDS).map(|_| Simulated::new(&memory)).collect();
        let mut world = World { memory, config, backends, vanished: Vec::new() };
        for op in ops {
            let before = table(&world.memory);
            world.apply(op)?;
            world.check(&before)?;
        }
        world.settle()?;
        // A vanished backend's exit stands in for the restart that frees
        // what it held.
        for mut backend in world.backends.into_iter().chain(world.vanished) {
            backend.port.exit();
        }
        world.memory.backend().evict(&|_| true);
        prop_assert!(world.memory.slots().iter().all(|slot| slot.state == State::Empty));
        prop_assert_eq!(world.memory.allocated(), 0, "every region is freed");
    }
}
