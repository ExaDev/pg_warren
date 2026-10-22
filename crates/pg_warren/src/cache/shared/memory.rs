//! The `Memory` adapter: an in-process fake of the shared cache. A
//! `Memory` is a cluster (a registry in a `Mutex`, a condition variable,
//! and heap regions standing in for DSM segments, freed when their last
//! `Arc` drops) and each `Backend` is one backend of it, usually on its own
//! thread. A backend reads a region through its own `Mapping`, as each
//! backend has its own mapping of a DSM segment, so that its session's
//! count of a mapping's holders is this backend's alone.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use super::admission::Limits;
use super::registry::{Claim, Gone, Lookup, Registry};
use super::session::{Release, Session, Use};
use super::slot::{Handle, Key, Pid, Slot, State, Ticket};
use super::{Claimed, Found, SharedVersions};

/// A cluster's shared cache.
#[derive(Clone)]
pub(crate) struct Memory {
    cluster: Arc<Cluster>,
}

struct Cluster {
    store: Mutex<Store>,
    /// Broadcast whenever a slot leaves `loading`.
    changed: Condvar,
    /// How long a waiter sleeps before it checks the loader is alive.
    wait: Duration,
}

/// What the registry's lock guards.
struct Store {
    registry: Registry<Vec<Slot>>,
    /// The published regions the registry pins, by handle: the
    /// segment's pin, apart from every backend's mapping.
    pinned: HashMap<Handle, Arc<Region>>,
    last_handle: u32,
    cap: u64,
    /// Where regions are created: how many bytes it holds.
    capacity: u64,
    /// The bytes of the regions alive.
    allocated: Arc<AtomicU64>,
    /// The backends alive.
    live: HashSet<Pid>,
    last_pid: Pid,
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
        let store = Store {
            registry: Registry::new(vec![Slot::EMPTY; slots]),
            pinned: HashMap::new(),
            last_handle: 0,
            cap,
            capacity,
            allocated: Arc::new(AtomicU64::new(0)),
            live: HashSet::new(),
            last_pid: 0,
        };
        let cluster = Cluster { store: Mutex::new(store), changed: Condvar::new(), wait };
        Self { cluster: Arc::new(cluster) }
    }

    /// A new backend.
    pub(crate) fn backend(&self) -> Backend {
        let mut store = self.cluster.lock();
        store.last_pid += 1;
        let pid = store.last_pid;
        store.live.insert(pid);
        Backend { cluster: Arc::clone(&self.cluster), session: Session::new(pid) }
    }

    /// The registry's slots now.
    pub(crate) fn slots(&self) -> Vec<Slot> {
        self.cluster.lock().registry.slots().to_vec()
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
    /// whose exit callback never ran would.
    pub(crate) fn vanish(self) {
        self.cluster.lock().live.remove(&self.session.pid());
    }
}

impl SharedVersions for Backend {
    type Region = Loading;
    type View = Mapping;

    fn lookup(&mut self, key: Key) -> Found<Mapping> {
        let mut store = self.cluster.lock();
        let ticket = match store.registry.lookup(key) {
            Lookup::Ready(ticket) => ticket,
            Lookup::Loading(ticket) => return Found::Loading(ticket),
            Lookup::Missing => return Found::Missing,
        };
        match self.session.open(&mut store.registry, ticket) {
            Ok(Use::Mapped(view)) => Found::Ready(view),
            Ok(Use::Attach(handle)) => {
                let mapping = Mapping::of(&store.pinned[&handle]);
                match self.session.mapped(&mut store.registry, ticket, mapping) {
                    Ok(view) => Found::Ready(view),
                    Err(Gone) => Found::Missing,
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
                    store.pinned.remove(&handle);
                }
                store.allocated.fetch_add(bytes, Ordering::SeqCst);
                let allocated = Arc::clone(&store.allocated);
                let region = Region { bytes: vec![0; bytes as usize], allocated };
                Claimed::Load(Loading { ticket, region })
            }
            Claim::Taken => Claimed::Taken,
            Claim::Refused(refusal) => Claimed::Refused(refusal),
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

    fn abandon(&mut self, loading: Loading) {
        let mut store = self.cluster.lock();
        self.session.abandon(&mut store.registry, loading.ticket);
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
        let gone = store.registry.loader(loading).is_some_and(|pid| !store.live.contains(&pid));
        if waited.timed_out() && gone {
            store.registry.abandon(loading);
            self.cluster.changed.notify_all();
        }
    }

    fn unpin(&mut self) {
        let mut store = self.cluster.lock();
        self.session.unpin(&mut store.registry);
    }

    fn release(&mut self, which: Release) {
        let mut store = self.cluster.lock();
        self.session.release(&mut store.registry, which, drop);
    }

    fn evict(&mut self, which: &dyn Fn(&Key) -> bool) -> usize {
        let mut store = self.cluster.lock();
        let evicted = store.registry.evict(which);
        for handle in &evicted {
            store.pinned.remove(handle);
        }
        evicted.len()
    }

    fn exit(&mut self) {
        let mut store = self.cluster.lock();
        self.session.exit(&mut store.registry, drop);
        store.live.remove(&self.session.pid());
        self.cluster.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
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
}
