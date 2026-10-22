//! The shared cache against a reference model (DESIGN.md §5.6): random
//! interleavings of what several backends do (read, finish or fail a
//! load, end a statement, drop what calls hold, release mappings, evict,
//! exit) on a `Memory` cluster, checking after each step that the cap
//! holds, that no pinned version is evicted, that each version has at most
//! one loading or ready slot, that every loading slot has a live loader
//! (so a failed loader's slot is retaken), that the registry's pins and
//! mappers are what the backends hold, that refusals are those of a simple
//! model of admission, and that every region is freed in the end.

use std::sync::Arc;
use std::time::Duration;

use pgrx::pg_sys::Oid;
use proptest::prelude::*;

use super::admission::Refusal;
use super::memory::{Backend, Loading, Mapping, Memory};
use super::session::Release;
use super::slot::{Key, Slot, State, Ticket};
use super::{Claimed, Found, SharedVersions};

const BACKENDS: usize = 3;
const VERSIONS: usize = 4;

/// What one backend does next.
#[derive(Debug, Clone)]
enum Op {
    /// Reads a version: finds it, waits (does nothing) or claims it.
    Read(usize, usize),
    /// Publishes the version it is loading.
    Publish(usize),
    /// Raises an ERROR, which drops the region it is loading, if any, and
    /// runs the abort callback.
    Fail(usize),
    /// Ends its statement.
    EndStatement(usize),
    /// Its calls stop holding what they read.
    DropViews(usize),
    /// Releases its mappings.
    Release(usize, Release),
    /// Evicts a version.
    Evict(usize, usize),
    /// Exits (FATAL); a new backend takes its place.
    Exit(usize),
}

fn op() -> impl Strategy<Value = Op> {
    let backend = 0..BACKENDS;
    let version = 0..VERSIONS;
    let release = prop_oneof![Just(Release::Evicted), Just(Release::Unheld)];
    prop_oneof![
        5 => (backend.clone(), version.clone()).prop_map(|(b, v)| Op::Read(b, v)),
        3 => backend.clone().prop_map(Op::Publish),
        1 => backend.clone().prop_map(Op::Fail),
        3 => backend.clone().prop_map(Op::EndStatement),
        2 => backend.clone().prop_map(Op::DropViews),
        3 => (backend.clone(), release).prop_map(|(b, r)| Op::Release(b, r)),
        1 => (backend.clone(), version).prop_map(|(b, v)| Op::Evict(b, v)),
        1 => backend.prop_map(Op::Exit),
    ]
}

fn key(version: usize) -> Key {
    Key { database: Oid::from(1), extension: Oid::from(2), version: version as i64 }
}

/// A backend and what its calls hold.
struct Simulated {
    port: Backend,
    /// The regions its calls read, by version.
    views: Vec<(usize, Arc<Mapping>)>,
    /// The version it is loading.
    pending: Option<(usize, Loading)>,
}

impl Simulated {
    fn new(memory: &Memory) -> Self {
        Self { port: memory.backend(), views: Vec::new(), pending: None }
    }
}

/// The cluster's configuration.
struct Config {
    cap: u64,
    capacity: u64,
    sizes: Vec<u64>,
}

/// The registry's slots and their pins now.
fn table(memory: &Memory) -> Vec<(Slot, u32)> {
    memory.slots().into_iter().zip(memory.pins()).collect()
}

/// What admission should decide for a version of `bytes` over `slots` and
/// their pins: `None` if it may be admitted (unless the store is full).
fn model_refusal(slots: &[(Slot, u32)], bytes: u64, cap: u64) -> Option<Refusal> {
    let reclaimable =
        |(slot, pins): &&(Slot, u32)| slot.state == State::Ready && *pins == 0 && slot.mappers == 0;
    let staying: u64 = slots
        .iter()
        .filter(|entry| entry.0.state != State::Empty && !reclaimable(entry))
        .map(|(slot, _)| slot.bytes)
        .sum::<u64>()
        + bytes;
    let free = slots.iter().any(|entry| entry.0.state == State::Empty || reclaimable(&entry));
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

/// Backend `b` reads `version`.
fn read(
    memory: &Memory,
    config: &Config,
    backend: &mut Simulated,
    version: usize,
) -> Result<(), TestCaseError> {
    if backend.pending.is_some() {
        return Ok(());
    }
    let before = table(memory);
    let allocated = memory.allocated();
    match backend.port.lookup(key(version)) {
        Found::Ready(view) => backend.views.push((version, view)),
        Found::Loading(_) => {}
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
                Claimed::Refused(refusal) => prop_assert_eq!(expected, Some(refusal)),
            }
        }
    }
    Ok(())
}

/// Applies `op`.
fn apply(
    memory: &Memory,
    config: &Config,
    backends: &mut [Simulated],
    op: Op,
) -> Result<(), TestCaseError> {
    match op {
        Op::Read(b, version) => read(memory, config, &mut backends[b], version)?,
        Op::Publish(b) => {
            let backend = &mut backends[b];
            if let Some((version, region)) = backend.pending.take() {
                let view = backend.port.publish(region);
                prop_assert!(view.is_ok(), "nothing takes a live loader's slot");
                if let Ok(view) = view {
                    backend.views.push((version, view));
                }
            }
        }
        Op::Fail(b) => {
            let backend = &mut backends[b];
            backend.pending = None;
            backend.port.abort();
        }
        Op::EndStatement(b) => backends[b].port.unpin(),
        Op::DropViews(b) => backends[b].views.clear(),
        Op::Release(b, which) => backends[b].port.release(which),
        Op::Evict(b, version) => {
            backends[b].port.evict(&|evicted| *evicted == key(version));
        }
        Op::Exit(b) => {
            let mut gone = std::mem::replace(&mut backends[b], Simulated::new(memory));
            gone.port.exit();
        }
    }
    Ok(())
}

/// The invariants, after a step from `before`.
fn check(
    memory: &Memory,
    config: &Config,
    backends: &[Simulated],
    before: &[(Slot, u32)],
) -> Result<(), TestCaseError> {
    let slots = memory.slots();
    let pinned = memory.pins();
    let occupied: u64 =
        slots.iter().filter(|slot| slot.state != State::Empty).map(|slot| slot.bytes).sum();
    prop_assert!(occupied <= config.cap, "{} bytes over a cap of {}", occupied, config.cap);
    for (index, ((was, pins), now)) in before.iter().zip(&slots).enumerate() {
        let still_ready = now.state == State::Ready && now.generation == was.generation;
        if was.state == State::Ready && !still_ready {
            prop_assert_eq!(*pins, 0, "slot {} was evicted while pinned", index);
        }
    }
    for version in 0..VERSIONS {
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
        let sessions = || backends.iter().map(|backend| backend.port.session());
        let pins = sessions().filter(|session| session.pinned().contains(&ticket)).count();
        let mappers =
            sessions().filter(|session| session.mapped_slots().any(|t| t == ticket)).count();
        prop_assert_eq!(pinned[index] as usize, pins, "pins of slot {}", index);
        prop_assert_eq!(slot.mappers as usize, mappers, "mappers of slot {}", index);
        if slot.state == State::Draining {
            prop_assert!(slot.mappers > 0, "slot {} drains with no mapper", index);
        }
        if slot.state == State::Loading {
            let loaders: Vec<_> =
                sessions().filter(|session| session.loading().contains(&ticket)).collect();
            prop_assert_eq!(loaders.len(), 1, "slot {} has no live loader", index);
            prop_assert_eq!(loaders[0].process(), slot.loader);
        }
    }
    for backend in backends {
        for ticket in backend.port.session().pinned() {
            prop_assert_eq!(slots[ticket.slot].state, State::Ready);
        }
        for (version, view) in &backend.views {
            let bytes = config.sizes[*version] as usize;
            prop_assert_eq!(view.bytes(), &vec![*version as u8 + 1; bytes][..]);
        }
    }
    Ok(())
}

proptest! {
    #[test]
    fn the_shared_cache_keeps_its_invariants(
        slots in 1usize..6,
        cap in prop_oneof![1 => Just(0u64), 9 => 20u64..200],
        capacity in prop_oneof![3 => Just(1000u64), 1 => 0u64..300],
        sizes in prop::collection::vec(1u64..70, VERSIONS),
        ops in prop::collection::vec(op(), 1..80),
    ) {
        let memory = Memory::new(slots, cap, capacity, Duration::ZERO);
        let config = Config { cap, capacity, sizes };
        let mut backends: Vec<Simulated> = (0..BACKENDS).map(|_| Simulated::new(&memory)).collect();
        for op in ops {
            let before = table(&memory);
            apply(&memory, &config, &mut backends, op)?;
            check(&memory, &config, &backends, &before)?;
        }
        for mut backend in backends {
            backend.port.exit();
        }
        memory.backend().evict(&|_| true);
        prop_assert!(memory.slots().iter().all(|slot| slot.state == State::Empty));
        prop_assert_eq!(memory.allocated(), 0, "every region is freed");
    }
}
