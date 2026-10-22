//! Reading a version through the shared cache (DESIGN.md §5.6): look it
//! up; wait while another backend loads it; on a miss, claim it, load it
//! and publish it; take over a load its loader gave up. One loader per
//! version, whatever the number of backends that miss at once.

use std::sync::Arc;

use super::admission::Refusal;
use super::slot::Key;
use super::{Claimed, Found, SharedVersions};

/// Where a version is read from.
#[derive(Debug)]
pub(crate) enum Acquired<V> {
    /// The shared cache, pinned for the current statement.
    Shared(Arc<V>),
    /// The backend's own cache: the shared cache refused it.
    Private(Refusal),
}

/// Version `key` from the shared cache. If no other backend has it or is
/// loading it, `plan` sizes it (its region's bytes, and what `load` needs
/// to fill it), once, and `load` fills a claimed region. A load
/// that fails returns its error and drops its region, as an ERROR unwinds
/// through it; the claim stays until the transaction's abort callback
/// calls `SharedVersions::abort`, and a waiter then takes it over.
pub(crate) fn acquire<P: SharedVersions, T, E>(
    port: &mut P,
    key: Key,
    mut plan: impl FnMut() -> Result<(u64, T), E>,
    mut load: impl FnMut(&T, &mut P::Region) -> Result<(), E>,
) -> Result<Acquired<P::View>, E> {
    let mut planned = None;
    loop {
        match port.lookup(key) {
            Found::Ready(view) => return Ok(Acquired::Shared(view)),
            Found::Loading(loading) => port.wait(loading),
            Found::Missing => {
                let made = match planned.take() {
                    Some(made) => made,
                    None => plan()?,
                };
                let (bytes, made) = planned.insert(made);
                match port.claim(key, *bytes) {
                    Claimed::Load(mut region) => {
                        load(made, &mut region)?;
                        if let Ok(view) = port.publish(region) {
                            return Ok(Acquired::Shared(view));
                        }
                    }
                    Claimed::Taken => {}
                    Claimed::Refused(refusal) => return Ok(Acquired::Private(refusal)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Barrier};
    use std::thread;
    use std::time::Duration;

    use pgrx::pg_sys::Oid;

    use super::*;
    use crate::cache::shared::memory::{Loading, Mapping, Memory};
    use crate::cache::shared::slot::State;

    const KEY: Key = Key { database: Oid::INVALID, extension: Oid::INVALID, version: 7 };

    fn cluster(cap: u64) -> Memory {
        Memory::new(4, cap, u64::MAX, Duration::from_millis(10))
    }

    /// A plan of `bytes` bytes.
    fn sized(bytes: u64) -> impl FnMut() -> Result<(u64, ()), String> {
        move || Ok((bytes, ()))
    }

    /// Writes the version's bytes.
    fn fill(region: &mut Loading) -> Result<(), String> {
        region.bytes_mut().fill(7);
        Ok(())
    }

    fn shared(acquired: Acquired<Mapping>) -> Arc<Mapping> {
        match acquired {
            Acquired::Shared(view) => view,
            Acquired::Private(refusal) => panic!("refused: {refusal:?}"),
        }
    }

    #[test]
    fn a_miss_is_loaded_and_published_once_then_found() {
        let memory = cluster(100);
        let mut loader = memory.backend();
        let loads = AtomicUsize::new(0);
        let mut counted = |region: &mut Loading| {
            loads.fetch_add(1, Ordering::SeqCst);
            fill(region)
        };
        let view =
            shared(acquire(&mut loader, KEY, sized(10), |_, region| counted(region)).unwrap());
        assert_eq!(view.bytes(), [7; 10]);
        let mut reader = memory.backend();
        let found =
            shared(acquire(&mut reader, KEY, sized(10), |_, region| counted(region)).unwrap());
        assert_eq!(view.bytes().as_ptr(), found.bytes().as_ptr(), "one region");
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_version_found_is_neither_planned_nor_loaded() {
        let memory = cluster(100);
        drop(shared(acquire(&mut memory.backend(), KEY, sized(10), |_, r| fill(r)).unwrap()));
        let unplanned = || -> Result<(u64, ()), String> { panic!("planned") };
        let found = acquire(&mut memory.backend(), KEY, unplanned, |_, _| panic!("loaded"));
        assert_eq!(shared(found.unwrap()).bytes(), [7; 10]);
    }

    #[test]
    fn a_refused_version_is_read_privately_without_loading() {
        let memory = cluster(0);
        let mut backend = memory.backend();
        let acquired =
            acquire(&mut backend, KEY, sized(10), |_, _| Err("loaded".to_string())).unwrap();
        assert!(matches!(acquired, Acquired::Private(Refusal::Disabled)));
    }

    #[test]
    fn a_failed_load_returns_its_error_and_the_abort_gives_it_up() {
        let memory = cluster(100);
        let mut backend = memory.backend();
        let failed = acquire(&mut backend, KEY, sized(10), |_, _| Err("cancelled".to_string()));
        assert_eq!(failed.err(), Some("cancelled".to_string()));
        assert_eq!(memory.allocated(), 0, "its region is dropped with the error");
        assert!(matches!(backend.lookup(KEY), Found::Loading(_)), "until the abort");
        backend.abort();
        assert!(memory.slots().iter().all(|slot| slot.state == State::Empty));
        assert!(matches!(backend.lookup(KEY), Found::Missing));
        assert_eq!(memory.allocated(), 0, "its region is freed");
    }

    #[test]
    fn concurrent_misses_load_once() {
        let memory = cluster(100);
        let loads = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(8));
        let readers: Vec<_> = (0..8)
            .map(|_| {
                let (mut backend, loads, start) =
                    (memory.backend(), Arc::clone(&loads), Arc::clone(&start));
                thread::spawn(move || {
                    start.wait();
                    let view = shared(
                        acquire(&mut backend, KEY, sized(10), |_, region| {
                            loads.fetch_add(1, Ordering::SeqCst);
                            thread::sleep(Duration::from_millis(50));
                            fill(region)
                        })
                        .unwrap(),
                    );
                    assert_eq!(view.bytes(), [7; 10]);
                    backend.unpin();
                })
            })
            .collect();
        for reader in readers {
            reader.join().unwrap();
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        let ready: Vec<_> = memory
            .slots()
            .into_iter()
            .zip(memory.pins())
            .filter(|(slot, _)| slot.mappers > 0)
            .map(|(slot, pins)| (pins, slot.mappers))
            .collect();
        assert_eq!(ready, [(0, 8)], "each reader maps it once");
    }

    #[test]
    fn a_waiter_takes_over_an_aborted_load() {
        let memory = cluster(100);
        let (claimed, wait_for_claim) = mpsc::channel();
        let (fail, failing) = mpsc::channel::<()>();
        let mut first = memory.backend();
        let loader = thread::spawn(move || {
            acquire(&mut first, KEY, sized(10), |_, _| {
                claimed.send(()).unwrap();
                failing.recv().unwrap();
                Err("cancelled".to_string())
            })
            .inspect_err(|_| first.abort())
            .err()
        });
        wait_for_claim.recv().unwrap();
        let mut second = memory.backend();
        let waiter = thread::spawn(move || {
            shared(acquire(&mut second, KEY, sized(10), |_, r| fill(r)).unwrap())
        });
        thread::sleep(Duration::from_millis(30));
        fail.send(()).unwrap();
        assert_eq!(loader.join().unwrap(), Some("cancelled".to_string()));
        assert_eq!(waiter.join().unwrap().bytes(), [7; 10], "the waiter loaded it");
    }

    #[test]
    fn a_waiter_takes_over_the_load_of_a_vanished_loader() {
        let memory = cluster(100);
        let mut lost = memory.backend();
        let Claimed::Load(_region) = lost.claim(KEY, 10) else {
            panic!("not claimed");
        };
        lost.vanish();
        let mut waiter = memory.backend();
        assert_eq!(
            shared(acquire(&mut waiter, KEY, sized(10), |_, r| fill(r)).unwrap()).bytes(),
            [7; 10]
        );
    }
}
