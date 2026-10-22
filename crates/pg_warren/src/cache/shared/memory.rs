//! The `Memory` adapter: an in-process fake of the shared cache. A
//! `Memory` is a cluster (a registry in a `Mutex`, its pin table outside
//! it, which a statement's end updates without the lock, a condition
//! variable, and heap regions standing in for DSM segments, freed when
//! their last `Arc` drops) and each `Backend` is one backend of it, usually
//! on its own thread. A backend reads a region through its own `Mapping`, as each
//! backend has its own mapping of a DSM segment, so that its session's
//! count of a mapping's holders is this backend's alone.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::Duration;

use super::admission::Limits;
use super::registry::{Claim, Gone, Lookup, Registry};
use super::session::{Mapped, Release, Session, Unattached, Use};
use super::slot::{Handle, Key, Process, Slot, Started, State, Ticket};
use super::{Claimed, Found, SharedVersions};

/// A cluster's shared cache.
#[derive(Clone)]
pub(crate) struct Memory {
    cluster: Arc<Cluster>,
}

struct Cluster {
    store: Mutex<Store>,
    /// The registry's pin table, which `unpin` uses without the lock.
    pins: Arc<[AtomicU32]>,
    /// Broadcast whenever a slot leaves `loading`.
    changed: Condvar,
    /// How long a waiter sleeps before it checks the loader is alive.
    wait: Duration,
}

/// What the registry's lock guards.
struct Store {
    registry: Registry<Vec<Slot>, Arc<[AtomicU32]>>,
    /// The published regions the registry pins, by handle: the
    /// segment's pin, apart from every backend's mapping.
    pinned: HashMap<Handle, Arc<Region>>,
    /// The regions evicted while mapped, alive until their last mapping
    /// drops, by handle: a reader revives one by pinning it again.
    unpinned: HashMap<Handle, Weak<Region>>,
    last_handle: u32,
    cap: u64,
    /// Where regions are created: how many bytes it holds.
    capacity: u64,
    /// The bytes of the regions alive.
    allocated: Arc<AtomicU64>,
    /// The backends alive. A new backend takes the lowest pid none of
    /// them has, so pids are reused, as an operating system reuses them.
    live: HashSet<Process>,
    /// The last backend's start, a counter.
    last_started: Started,
}

/// A region: a version's bytes, counted in its store while alive.
#[derive(Debug)]
pub(crate) struct Region {
    bytes: Vec<u8>,
    allocated: Arc<AtomicU64>,
}

impl Region {
    /// Its bytes.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        self.allocated.fetch_sub(self.bytes.len() as u64, Ordering::SeqCst);
    }
}

/// A backend's mapping of a published region.
#[derive(Debug)]
pub(crate) struct Mapping {
    region: Arc<Region>,
}

impl Mapping {
    /// Maps `region` in a backend.
    fn of(region: &Arc<Region>) -> Arc<Self> {
        Arc::new(Self { region: Arc::clone(region) })
    }

    /// The region's bytes.
    pub(crate) fn bytes(&self) -> &[u8] {
        self.region.bytes()
    }
}

impl Mapped for Mapping {
    /// Calls hold a `Memory` mapping only through its `Arc`.
    fn in_use(&self) -> bool {
        false
    }
}

/// A region claimed to load a version into.
#[derive(Debug)]
pub(crate) struct Loading {
    ticket: Ticket,
    region: Region,
}

impl Loading {
    /// The bytes to load the version into.
    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.region.bytes
    }
}

/// One backend of a `Memory` cluster.
pub(crate) struct Backend {
    cluster: Arc<Cluster>,
    session: Session<Mapping>,
}

impl Memory {
    /// A cluster with `slots` slots, a cap of `cap` bytes, and `capacity`
    /// bytes where regions are created; a waiter checks its loader every
    /// `wait`.
    pub(crate) fn new(slots: usize, cap: u64, capacity: u64, wait: Duration) -> Self {
        let pins: Arc<[AtomicU32]> = (0..slots).map(|_| AtomicU32::new(0)).collect();
        let store = Store {
            registry: Registry::new(vec![Slot::EMPTY; slots], Arc::clone(&pins)),
            pinned: HashMap::new(),
            unpinned: HashMap::new(),
            last_handle: 0,
            cap,
            capacity,
            allocated: Arc::new(AtomicU64::new(0)),
            live: HashSet::new(),
            last_started: 0,
        };
        let cluster = Cluster { store: Mutex::new(store), pins, changed: Condvar::new(), wait };
        Self { cluster: Arc::new(cluster) }
    }

    /// A new backend.
    pub(crate) fn backend(&self) -> Backend {
        let mut store = self.cluster.lock();
        let pid = (1..).find(|&pid| store.live.iter().all(|live| live.pid != pid));
        store.last_started += 1;
        let process = Process { pid: pid.unwrap_or_default(), started: store.last_started };
        store.live.insert(process);
        Backend { cluster: Arc::clone(&self.cluster), session: Session::new(process) }
    }

    /// The registry's slots now.
    pub(crate) fn slots(&self) -> Vec<Slot> {
        self.cluster.lock().registry.slots().to_vec()
    }

    /// The registry's pins now, read without its lock.
    pub(crate) fn pins(&self) -> Vec<u32> {
        self.cluster.pins.iter().map(|count| count.load(Ordering::Acquire)).collect()
    }

    /// The bytes of the regions alive.
    pub(crate) fn allocated(&self) -> u64 {
        self.cluster.lock().allocated.load(Ordering::SeqCst)
    }
}

impl Cluster {
    fn lock(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap()
    }
}

impl Store {
    /// Unpins an evicted region: it lives while a mapping holds it.
    fn unpin(&mut self, handle: Handle) {
        if let Some(region) = self.pinned.remove(&handle) {
            self.unpinned.insert(handle, Arc::downgrade(&region));
        }
    }

    /// Pins a draining slot's region again and revives the slot.
    fn revive(&mut self, ticket: Ticket) -> Result<(), Gone> {
        let handle = self.registry.slots()[ticket.slot].handle;
        let region = self.unpinned.remove(&handle).and_then(|region| region.upgrade());
        let region = region.expect("a draining region is mapped, so alive");
        self.registry.revive(ticket)?;
        self.pinned.insert(handle, region);
        Ok(())
    }

    fn limits(&self) -> Limits {
        let allocated = self.allocated.load(Ordering::SeqCst);
        Limits { cap: self.cap, store_room: self.capacity.saturating_sub(allocated) }
    }
}

impl Backend {
    /// The backend's session.
    pub(crate) fn session(&self) -> &Session<Mapping> {
        &self.session
    }

    /// Leaves the cluster without giving back what it holds, as a backend
    /// whose exit callback never ran would; it does nothing more.
    pub(crate) fn vanish(&mut self) {
        self.cluster.lock().live.remove(&self.session.process());
    }

    /// Looks `key` up as `lookup` does, but the attach raises an ERROR, as
    /// a failing `dsm_attach` would: whether it got as far as the attach.
    /// The caller runs the abort callback.
    pub(crate) fn fail_attach(&mut self, key: Key) -> bool {
        let mut store = self.cluster.lock();
        let Lookup::Ready(ticket) = store.registry.lookup(key) else {
            return false;
        };
        matches!(self.session.open(&mut store.registry, ticket), Ok(Use::Attach(_)))
    }
}

impl SharedVersions for Backend {
    type Region = Loading;
    type View = Mapping;

    fn lookup(&mut self, key: Key) -> Found<Mapping> {
        let mut store = self.cluster.lock();
        let ticket = match store.registry.lookup(key) {
            Lookup::Ready(ticket) => ticket,
            Lookup::Draining(ticket) => match store.revive(ticket) {
                Ok(()) => ticket,
                Err(Gone) => return Found::Missing,
            },
            Lookup::Loading(ticket) => return Found::Loading(ticket),
            Lookup::Missing => return Found::Missing,
        };
        match self.session.open(&mut store.registry, ticket) {
            Ok(Use::Mapped(view)) => Found::Ready(view),
            Ok(Use::Attach(handle)) => {
                let mapping = Mapping::of(&store.pinned[&handle]);
                match self.session.mapped(&mut store.registry, ticket, mapping) {
                    Ok(view) => Found::Ready(view),
                    // Its region is freed with its last `Arc`.
                    Err(Unattached(_)) => Found::Missing,
                }
            }
            Err(Gone) => Found::Missing,
        }
    }

    fn claim(&mut self, key: Key, bytes: u64) -> Claimed<Loading> {
        let mut store = self.cluster.lock();
        let limits = store.limits();
        match self.session.claim(&mut store.registry, key, bytes, limits) {
            Claim::Load { ticket, evicted } => {
                for handle in evicted {
                    store.unpin(handle);
                }
                store.allocated.fetch_add(bytes, Ordering::SeqCst);
                let allocated = Arc::clone(&store.allocated);
                let region = Region { bytes: vec![0; bytes as usize], allocated };
                Claimed::Load(Loading { ticket, region })
            }
            Claim::Taken => Claimed::Taken,
            Claim::Refused { refusal, drained } => {
                for handle in drained {
                    store.unpin(handle);
                }
                Claimed::Refused(refusal)
            }
        }
    }

    fn publish(&mut self, loading: Loading) -> Result<Arc<Mapping>, Gone> {
        let mut store = self.cluster.lock();
        store.last_handle += 1;
        let handle = Handle(store.last_handle);
        let region = Arc::new(loading.region);
        let mapping = Mapping::of(&region);
        let view = self.session.publish(&mut store.registry, loading.ticket, handle, mapping)?;
        store.pinned.insert(handle, region);
        self.cluster.changed.notify_all();
        Ok(view)
    }

    fn abort(&mut self) {
        let mut store = self.cluster.lock();
        self.session.abort(&mut store.registry);
        self.cluster.changed.notify_all();
    }

    fn wait(&mut self, loading: Ticket) {
        let store = self.cluster.lock();
        let (mut store, waited) = self
            .cluster
            .changed
            .wait_timeout_while(store, self.cluster.wait, |store| {
                store.registry.state(loading) == Some(State::Loading)
            })
            .unwrap();
        let gone =
            store.registry.loader(loading).is_some_and(|loader| !store.live.contains(&loader));
        if waited.timed_out() && gone {
            store.registry.abandon(loading);
            self.cluster.changed.notify_all();
        }
    }

    fn unpin(&mut self) {
        self.session.unpin(&self.cluster.pins);
    }

    fn release(&mut self, which: Release) {
        let mut store = self.cluster.lock();
        self.session.release(&mut store.registry, which, drop);
    }

    fn evict(&mut self, which: &dyn Fn(&Key) -> bool) -> usize {
        let mut store = self.cluster.lock();
        let evicted = store.registry.evict(which);
        for &handle in &evicted {
            store.unpin(handle);
        }
        evicted.len()
    }

    fn forget(&mut self, which: &dyn Fn(&Key) -> bool) {
        let mut store = self.cluster.lock();
        self.session.forget(&mut store.registry, which, drop);
    }

    fn keys(&mut self) -> Vec<Key> {
        self.cluster.lock().registry.keys()
    }

    fn exit(&mut self) {
        let mut store = self.cluster.lock();
        self.session.exit(&mut store.registry, drop);
        store.live.remove(&self.session.process());
        self.cluster.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use pgrx::pg_sys::Oid;

    use super::*;

    fn key(version: i64) -> Key {
        Key { database: Oid::from(1), extension: Oid::from(2), version }
    }

    fn cluster() -> Memory {
        Memory::new(2, 100, u64::MAX, Duration::ZERO)
    }

    /// Claims and publishes `version` in `backend`; the view its call
    /// holds.
    fn published(backend: &mut Backend, version: i64) -> Arc<Mapping> {
        let Claimed::Load(region) = backend.claim(key(version), 10) else {
            panic!("not claimed");
        };
        backend.publish(region).unwrap()
    }

    fn mappers(memory: &Memory) -> u32 {
        memory.slots()[0].mappers
    }

    #[test]
    fn a_vanished_loader_whose_pid_is_reused_is_taken_over() {
        let memory = cluster();
        let mut lost = memory.backend();
        let Claimed::Load(_region) = lost.claim(key(1), 10) else {
            panic!("not claimed");
        };
        let Found::Loading(loading) = memory.backend().lookup(key(1)) else {
            panic!("not loading");
        };
        let pid = lost.session().process().pid;
        lost.vanish();
        let reused = memory.backend();
        assert_eq!(reused.session().process().pid, pid, "a new backend has the loader's pid");
        memory.backend().wait(loading);
        assert!(matches!(memory.backend().lookup(key(1)), Found::Missing), "taken over");
    }

    #[test]
    fn a_statement_ends_its_pins_without_the_registry_lock() {
        let memory = cluster();
        let mut backend = memory.backend();
        drop(published(&mut backend, 1));
        let (ended, end) = mpsc::channel();
        let locked = memory.cluster.lock();
        let unpinning = thread::spawn(move || {
            backend.unpin();
            ended.send(()).unwrap();
            backend
        });
        let waited = end.recv_timeout(Duration::from_secs(5));
        drop(locked);
        unpinning.join().unwrap();
        assert!(waited.is_ok(), "the unpin waited for the registry's lock");
        assert_eq!(memory.pins()[0], 0);
    }

    #[test]
    fn an_unheld_mapping_of_a_ready_version_is_released() {
        let memory = cluster();
        let mut backend = memory.backend();
        drop(published(&mut backend, 1));
        backend.unpin();
        backend.release(Release::Unheld);
        assert_eq!(mappers(&memory), 0);
        assert_eq!(backend.session().mapped_slots().count(), 0);
    }

    #[test]
    fn a_draining_version_mapped_by_two_backends_empties_once_both_release() {
        let memory = cluster();
        let (mut first, mut second) = (memory.backend(), memory.backend());
        drop(published(&mut first, 1));
        let Found::Ready(view) = second.lookup(key(1)) else {
            panic!("not found");
        };
        drop(view);
        first.unpin();
        second.unpin();
        assert_eq!(first.evict(&|_| true), 1);
        first.release(Release::Evicted);
        second.release(Release::Evicted);
        assert!(memory.slots().iter().all(|slot| slot.state == State::Empty));
        assert_eq!(memory.allocated(), 0, "its region is freed");
    }

    #[test]
    fn a_draining_version_read_again_is_revived_not_reloaded() {
        // A cap of two versions: a third refused drains the older, which
        // its next reader revives instead of draining the newer to load it.
        let memory = Memory::new(3, 25, u64::MAX, Duration::ZERO);
        let (mut first, mut second) = (memory.backend(), memory.backend());
        drop(published(&mut first, 1));
        drop(published(&mut first, 2));
        first.unpin();
        assert!(matches!(second.claim(key(3), 10), Claimed::Refused(_)));
        assert_eq!(memory.slots()[0].state, State::Draining);
        let Found::Ready(view) = second.lookup(key(1)) else {
            panic!("not revived");
        };
        assert_eq!(view.bytes().len(), 10);
        let states: Vec<State> = memory.slots().iter().map(|slot| slot.state).collect();
        assert_eq!(states, [State::Ready, State::Ready, State::Empty]);
        assert_eq!(memory.allocated(), 20, "nothing reloaded");
        drop(view);
        second.unpin();
        first.release(Release::Evicted);
        assert_eq!(memory.slots()[0].mappers, 2, "a ready version stays mapped");
    }
}
