//! The `Dsm` adapter (DESIGN.md §5.6, §14 items 26–28): the registry in
//! shared memory (`placement`), one DSM segment per shared version
//! (`segment`), and this backend's side of them: its session (pins,
//! mappings and claims), the registry's lock around every transition but a
//! statement's unpin, the condition variables loads are waited on, and the
//! callbacks that give back what a failed or exiting backend holds
//! (`callbacks`).
//!
//! Only the main thread calls into Postgres (§6.9). This backend's state
//! is borrowed only around pure Rust and the registry's lock, never across
//! a call that may check for interrupts: a FATAL exits from inside such a
//! call, without unwinding, and the exit callback must then find the
//! state free to give everything back.

mod callbacks;
mod placement;
mod segment;

use std::any::Any;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use pgrx::{ereport, pg_sys, PgSqlErrorCode};
use warren_core::compute::AnyVector;

use super::admission::{Limits, Refusal};
use super::registry::{Claim, Evicted, Gone, Listing, Lookup, Registry};
use super::session::{Mapped, Session, Unattached, Use};
use super::slot::{Handle, Key, Process, Slot, State, Ticket};
use super::unavailable::Unavailable;
use super::{setting, Claimed, Found, SharedVersions};
use crate::activity::{count_shared_evictions, record_mapped};
use crate::cache::load::{read, read_vector, CachedIds, LoadError, LoadedVersion};
use crate::cache::region::Region;
use crate::catalog::VersionId;
use crate::vector::VectorRecord;
use placement::{Located, Shared, SLOTS};
use segment::{Segment, SegmentRegion};

pub(crate) use placement::request_preload;
#[cfg(any(feature = "pg17", feature = "pg18", test))]
pub(in crate::cache::shared) use placement::FORMAT;
pub(in crate::cache::shared) use segment::PREFIX;

/// How long a waiter sleeps on a loading slot before it checks that the
/// loader still exists, in milliseconds.
const WAIT_MS: i64 = 1000;

thread_local! {
    /// This backend's side of the shared cache, once it has used it.
    static BACKEND: RefCell<Option<Backend>> = const { RefCell::new(None) };
}

/// This backend's side of the shared cache.
struct Backend {
    located: Located,
    session: Session<Attached>,
    /// The registry's drain count at this backend's last release.
    seen_drains: u64,
    /// Whether that release left a mapping of a slot no longer ready (one
    /// a call held), to release later.
    pending: bool,
}

/// The shared cache through the registry and DSM segments: the port for
/// this backend, whose state is thread-local.
#[derive(Debug)]
pub(crate) struct Dsm(());

/// This backend's mapping of a published version's (or persisted
/// vector's) segment, and the loaded version (or vector) built over it,
/// kept while the mapping is.
pub(crate) struct Attached {
    segment: Segment,
    /// The bytes the segment maps, read once, at the mapping.
    bytes: u64,
    held: Mutex<Held>,
}

/// What a mapping keeps of the version built over its segment: the loaded
/// version, while no call needs it rebuilt, and the region every view of
/// it (and of any piece cloned out of it) keeps alive.
#[derive(Default)]
struct Held {
    loaded: Option<Arc<dyn Any + Send + Sync>>,
    region: Weak<Region>,
}

/// A segment claimed to load a version into, and, once it has loaded and
/// been validated, the loaded version built over it.
pub(crate) struct Loading {
    ticket: Ticket,
    segment: Segment,
    region: SegmentRegion,
    held: Held,
}

impl Dsm {
    /// The port of this backend, which does nothing if the backend has not
    /// used the shared cache: for the ends of statements and messages and
    /// the callbacks.
    pub(in crate::cache::shared) fn here() -> Self {
        Self(())
    }

    /// This backend's mapping of `key`'s version, if the current statement
    /// pins it already: read again without the registry.
    pub(crate) fn pinned(&self, key: Key) -> Option<Arc<Attached>> {
        with_backend(|backend| backend.session.pinned_mapping(key).cloned()).flatten()
    }

    /// The port, if this backend has used the shared cache: without
    /// finding or creating the registry, unlike [`Dsm::open`].
    pub(crate) fn used() -> Option<Self> {
        BACKEND.with_borrow(Option::is_some).then_some(Self(()))
    }

    /// The port, after finding (or on PG 17 and 18 creating) the registry
    /// on this backend's first use and registering the callbacks that give
    /// back what it holds; why the shared cache is unavailable otherwise.
    pub(crate) fn open() -> Result<Self, Unavailable> {
        if cfg!(target_endian = "big") {
            return Err(Unavailable::BigEndian);
        }
        let known = BACKEND.with_borrow(Option::is_some);
        if !known {
            let located = placement::locate()?;
            let process = Process {
                // SAFETY: this backend's pid and start, set before any query.
                pid: unsafe { pg_sys::MyProcPid },
                // SAFETY: as above.
                started: unsafe { pg_sys::MyStartTimestamp },
            };
            let session = Session::new(process);
            BACKEND.set(Some(Backend { located, session, seen_drains: 0, pending: false }));
            callbacks::register();
        }
        Ok(Self(()))
    }
}

/// Runs `f` on this backend's state, if it has used the shared cache and
/// the state is free (it is borrowed only around pure Rust and the
/// registry's lock).
fn with_backend<R>(f: impl FnOnce(&mut Backend) -> R) -> Option<R> {
    BACKEND.with(|backend| backend.try_borrow_mut().ok()?.as_mut().map(f))
}

/// Raises an internal ERROR for a broken invariant of the shared cache.
fn broken(what: &str) -> ! {
    ereport!(
        ERROR,
        PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
        format!("pg_warren's shared cache: {what}")
    );
}

impl Backend {
    fn shared(&self) -> *mut Shared {
        self.located.shared.as_ptr()
    }

    /// The condition variable of slot `slot`.
    fn changed(&self, slot: usize) -> *mut pg_sys::ConditionVariable {
        // SAFETY: the registry stays mapped; only Postgres touches the
        // condition variables, through this pointer.
        unsafe { std::ptr::addr_of_mut!((*self.shared()).changed[slot.min(SLOTS - 1)]) }
    }

    /// Runs `f` on the registry and this backend's session under the
    /// registry's lock, held exclusively. `f` is pure Rust, or calls only
    /// Postgres functions that check for no interrupts (`dsm_detach`;
    /// `dsm_unpin_segment` to evict a version that drains, `unpin_draining`;
    /// and `dsm_attach` and `dsm_pin_segment` to revive a draining
    /// version).
    fn locked<R>(
        &mut self,
        f: impl FnOnce(
            &mut Registry<&mut [Slot; SLOTS], &[AtomicU32; SLOTS]>,
            &mut Session<Attached>,
        ) -> R,
    ) -> R {
        let (shared, lock) = (self.shared(), self.located.lock.as_ptr());
        // SAFETY: the registry's lock, on the main thread; LWLockAcquire
        // holds interrupts off while it waits.
        unsafe { pg_sys::LWLockAcquire(lock, pg_sys::LWLockMode::LW_EXCLUSIVE) };
        // SAFETY: the lock, held exclusively, gives this backend sole use of
        // the clocks and slots (every other backend changes them only under
        // it); the pins are atomics others may change meanwhile, so they
        // are only shared.
        let result = unsafe {
            let mut registry =
                Registry::resume((*shared).clocks, &mut (*shared).slots, &(*shared).pins);
            let result = f(&mut registry, &mut self.session);
            record_mapped(self.session.mappings().map(|mapped| mapped.bytes).sum());
            (*shared).clocks = registry.clocks();
            (*shared).drains.store(registry.clocks().drains(), Ordering::Release);
            result
        };
        // SAFETY: released as it was taken, on the main thread.
        unsafe { pg_sys::LWLockRelease(lock) };
        result
    }

    /// The registry's drain count, read without its lock.
    fn drains(&self) -> u64 {
        // SAFETY: the registry stays mapped; the count is an atomic, which
        // any backend reads through a shared reference.
        unsafe { (*self.shared()).drains.load(Ordering::Acquire) }
    }

    /// Releases mappings through `release`, under the registry's lock, and
    /// notes the drains seen and whether a mapping of a slot no longer
    /// ready is left (one a call holds), for the next release of evicted
    /// mappings.
    fn releasing(
        &mut self,
        release: impl FnOnce(
            &mut Registry<&mut [Slot; SLOTS], &[AtomicU32; SLOTS]>,
            &mut Session<Attached>,
        ),
    ) {
        let (drains, pending) = self.locked(|registry, session| {
            release(registry, session);
            let pending =
                session.mapped_slots().any(|mapped| registry.state(mapped) != Some(State::Ready));
            (registry.clocks().drains(), pending)
        });
        (self.seen_drains, self.pending) = (drains, pending);
    }

    /// Wakes the backends waiting on slot `slot`.
    fn broadcast(&self, slot: usize) {
        // SAFETY: a condition variable of the registry, on the main thread.
        unsafe { pg_sys::ConditionVariableBroadcast(self.changed(slot)) };
    }
}

/// What a lookup found, under the lock.
enum Step {
    Found(Found<Attached>),
    Attach(Ticket, Handle),
}

impl SharedVersions for Dsm {
    type Region = Loading;
    type View = Attached;

    fn lookup(&mut self, key: Key) -> Found<Attached> {
        let step = with_backend(|backend| {
            backend.locked(|registry, session| match registry.lookup(key) {
                Lookup::Missing => Step::Found(Found::Missing),
                Lookup::Loading(ticket) => Step::Found(Found::Loading(ticket)),
                Lookup::Draining(ticket) => Step::Found(revive(registry, session, key, ticket)),
                Lookup::Ready(ticket) => match session.open(registry, ticket) {
                    Ok(Use::Mapped(view)) => Step::Found(Found::Ready(view)),
                    Ok(Use::Attach(handle)) => Step::Attach(ticket, handle),
                    Err(Gone) => Step::Found(Found::Missing),
                },
            })
        });
        let (ticket, handle) = match step {
            Some(Step::Found(found)) => return found,
            Some(Step::Attach(ticket, handle)) => (ticket, handle),
            None => broken("this backend's state is in use"),
        };
        // The statement's pin keeps the slot ready, and so its segment.
        let Some(segment) = segment::attach(handle, key) else {
            broken("a ready version's segment cannot be attached or holds another version")
        };
        let attached = Arc::new(Attached::new(segment, Held::default()));
        let mapped = with_backend(|backend| {
            backend.locked(|registry, session| session.mapped(registry, ticket, attached))
        });
        match mapped {
            Some(Ok(view)) => Found::Ready(view),
            Some(Err(Unattached(attached))) => {
                // Detached first: an ERROR leaves no mapping behind.
                attached.detach();
                broken("a version pinned by this statement stopped being ready")
            }
            None => broken("this backend's state is in use"),
        }
    }

    fn claim(&mut self, key: Key, bytes: u64) -> Claimed<Loading> {
        let limits = Limits { cap: setting::cap(), store_room: segment::store_room() };
        let claim = with_backend(|backend| {
            backend.locked(|registry, session| {
                let claim = session.claim(registry, key, bytes, limits);
                if let Claim::Load { evicted, .. } | Claim::Refused { drained: evicted, .. } =
                    &claim
                {
                    unpin_draining(evicted);
                }
                claim
            })
        });
        if let Some(Claim::Load { evicted, .. } | Claim::Refused { drained: evicted, .. }) = &claim
        {
            unpin_emptied(evicted);
        }
        match claim {
            Some(Claim::Load { ticket, evicted }) => {
                count_shared_evictions(evicted.len() as u64);
                match segment::create(bytes, key) {
                    Some(segment) => {
                        let region = segment.region();
                        Claimed::Load(Loading { ticket, segment, region, held: Held::default() })
                    }
                    None => {
                        self.abort();
                        Claimed::Refused(Refusal::NoSegment)
                    }
                }
            }
            Some(Claim::Taken) => Claimed::Taken,
            Some(Claim::Refused { refusal, drained }) => {
                count_shared_evictions(drained.len() as u64);
                Claimed::Refused(refusal)
            }
            None => broken("this backend's state is in use"),
        }
    }

    fn publish(&mut self, loading: Loading) -> Result<Arc<Attached>, Gone> {
        let Loading { ticket, segment, held, .. } = loading;
        segment.keep();
        let handle = segment.handle();
        let attached = Arc::new(Attached::new(segment, held));
        let published = with_backend(|backend| {
            let published = backend
                .locked(|registry, session| session.publish(registry, ticket, handle, attached));
            backend.broadcast(ticket.slot);
            published
        });
        match published {
            Some(Ok(view)) => Ok(view),
            _ => {
                segment::unpin(handle);
                segment.detach();
                Err(Gone)
            }
        }
    }

    fn abort(&mut self) {
        with_backend(|backend| {
            let loading = backend.session.loading().to_vec();
            if !loading.is_empty() {
                backend.locked(|registry, session| session.abort(registry));
                loading.iter().for_each(|ticket| backend.broadcast(ticket.slot));
            }
        });
    }

    fn wait(&mut self, loading: Ticket) {
        let Some(changed) = with_backend(|backend| backend.changed(loading.slot)) else {
            return;
        };
        // SAFETY: a condition variable of the registry, on the main thread;
        // prepared before the state is checked, so no broadcast is missed.
        unsafe { pg_sys::ConditionVariablePrepareToSleep(changed) };
        let still = |backend: &mut Backend| {
            backend.locked(|registry, _| registry.state(loading) == Some(State::Loading))
        };
        if with_backend(still) == Some(true) {
            // SAFETY: as above; it checks for interrupts, so a cancel or
            // `statement_timeout` ends the wait with an ERROR, after which
            // the transaction's abort cancels the sleep.
            let timed_out =
                unsafe { pg_sys::ConditionVariableTimedSleep(changed, WAIT_MS, wait_event()) };
            if timed_out {
                take_over_if_gone(loading);
            }
        }
        // SAFETY: ends the sleep prepared above.
        unsafe { pg_sys::ConditionVariableCancelSleep() };
    }

    /// Pure Rust, so that it may run at the end of a transaction (DESIGN.md
    /// §5.5).
    fn unpin(&mut self) {
        with_backend(|backend| {
            // SAFETY: the registry stays mapped for the session; its pins
            // are atomics, which any backend may update through a shared
            // reference. Borrowed apart from the session.
            let pins = unsafe { &(*backend.shared()).pins };
            backend.session.unpin(pins);
        });
    }

    /// Takes the registry's lock and detaches segments, so never at a
    /// transaction's end.
    /// Releasing evicted mappings takes the lock only if a slot has
    /// started draining since this backend last released, or that release
    /// left one held: so a message's start costs one atomic read.
    fn release(&mut self) {
        with_backend(|backend| {
            if backend.session.mapped_slots().next().is_none() {
                return;
            }
            let drains = backend.drains();
            if !release_due(drains, backend.seen_drains, backend.pending) {
                return;
            }
            backend.releasing(|registry, session| {
                session.release(registry, |attached| attached.detach());
            });
        });
    }

    /// Takes the registry's lock and detaches segments, so never at a
    /// transaction's end.
    fn forget(&mut self, which: &dyn Fn(&Key) -> bool) {
        with_backend(|backend| {
            if backend.session.mapped_slots().next().is_some() {
                backend.releasing(|registry, session| {
                    session.forget(registry, which, |attached| attached.detach());
                });
            }
        });
    }

    fn keys(&mut self) -> Vec<Key> {
        with_backend(|backend| backend.locked(|registry, _| registry.keys())).unwrap_or_default()
    }

    fn listing(&mut self) -> Listing {
        with_backend(|backend| backend.locked(|registry, _| registry.listing())).unwrap_or_default()
    }

    fn evict(&mut self, which: &dyn Fn(&Key) -> bool) -> usize {
        let evicted = with_backend(|backend| {
            backend.locked(|registry, _| {
                let evicted = registry.evict(which);
                unpin_draining(&evicted);
                evicted
            })
        })
        .unwrap_or_default();
        unpin_emptied(&evicted);
        count_shared_evictions(evicted.len() as u64);
        evicted.len()
    }

    /// Postgres detaches the mappings after the exit callbacks
    /// (`dsm_backend_shutdown`), so nothing here unmaps memory a call might
    /// still hold. A FATAL can exit while compute threads still read a
    /// mapping (from the main thread's poll, without unwinding); the
    /// compute pool's own exit callback (`interrupts::pool`) stops
    /// them first, so none reads after that detach.
    fn exit(&mut self) {
        with_backend(|backend| {
            let lock = backend.located.lock.as_ptr();
            // SAFETY: on the main thread; an exit never leaves the lock held
            // (it is taken only around pure Rust), but if it did, it is
            // released before it is taken again.
            if unsafe { pg_sys::LWLockHeldByMe(lock) } {
                // SAFETY: held by this backend, as just checked.
                unsafe { pg_sys::LWLockRelease(lock) };
            }
            let loading = backend.session.loading().to_vec();
            backend.locked(|registry, session| session.exit(registry, drop));
            loading.iter().for_each(|ticket| backend.broadcast(ticket.slot));
        });
    }
}

/// The wait event a backend waiting for another's load shows in
/// `pg_stat_activity` (DESIGN.md §5.7): `WarrenSharedLoad` on PG 17 and 18,
/// `Extension` on PG 15 and 16.
fn wait_event() -> u32 {
    crate::activity::wait_event(c"WarrenSharedLoad")
}

/// Unpins the evicted regions whose slots drain: under the registry's
/// lock, so that no reader revives a slot, pinning its region again,
/// before its region is unpinned. Some backend maps each, so unpinning
/// frees none, and the lock is held for no more than the unpin.
fn unpin_draining(evicted: &[Evicted]) {
    evicted.iter().filter_map(|evicted| evicted.draining()).for_each(segment::unpin);
}

/// Unpins the evicted regions whose slots are empty, which frees them
/// (`shm_unlink` and the pages): after the registry's lock is released, so
/// that no lookup waits for it. Nobody maps them, and an empty slot is
/// never revived.
fn unpin_emptied(evicted: &[Evicted]) {
    evicted.iter().filter_map(|evicted| evicted.emptied()).for_each(segment::unpin);
}

/// Whether a release of evicted mappings is due: a slot has started
/// draining since the last (`drains` against `seen`), or that release left
/// one held (`pending`).
fn release_due(drains: u64, seen: u64, pending: bool) -> bool {
    pending || drains != seen
}

/// Revives the draining slot `ticket` names, which holds `key`'s version,
/// and reads it in this statement, under the registry's lock: the slot is
/// draining, so some backend still maps its segment, and none can detach
/// it while the lock is held (backends detach under it; one that exits
/// has given its mapping back first), so this backend's mapping, its own
/// or one it attaches now, keeps the segment whole while it pins it again.
fn revive(
    registry: &mut Registry<&mut [Slot; SLOTS], &[AtomicU32; SLOTS]>,
    session: &mut Session<Attached>,
    key: Key,
    ticket: Ticket,
) -> Found<Attached> {
    let attached = match session.mapping(ticket) {
        Some(attached) => Arc::clone(attached),
        None => {
            let handle = registry.slots()[ticket.slot].handle;
            let Some(segment) = segment::attach(handle, key) else {
                broken("a draining version's segment cannot be attached or holds another version")
            };
            Arc::new(Attached::new(segment, Held::default()))
        }
    };
    attached.segment.repin();
    if registry.revive(ticket).is_err() {
        broken("a draining version stopped draining under the registry's lock");
    }
    match session.open(registry, ticket) {
        Ok(Use::Mapped(view)) => Found::Ready(view),
        Ok(Use::Attach(_)) => match session.mapped(registry, ticket, attached) {
            Ok(view) => Found::Ready(view),
            Err(_) => broken("a revived version stopped being ready under the registry's lock"),
        },
        Err(Gone) => broken("a revived version stopped being ready under the registry's lock"),
    }
}

/// Empties a loading slot whose loader no longer exists, so that a waiter
/// loads it: a belt to the abort and exit callbacks, which a backend
/// killed outright never runs (but then the postmaster resets shared
/// memory). A process is looked up by its pid, among live backends.
fn take_over_if_gone(loading: Ticket) {
    let loader = with_backend(|backend| backend.locked(|registry, _| registry.loader(loading)));
    let Some(Some(loader)) = loader else { return };
    // SAFETY: looks the pid up in the proc array, on the main thread.
    let alive = !unsafe { pg_sys::BackendPidGetProc(loader.pid) }.is_null();
    if !alive {
        with_backend(|backend| {
            if backend.locked(|registry, _| registry.abandon(loading)) {
                backend.broadcast(loading.slot);
            }
        });
    }
}

impl Mapped for Attached {
    /// Whether a call holds the loaded version, or anything read through
    /// it: a piece cloned out of it (an id map, a CSR) shares its views of
    /// the region, not the loaded version. When no call holds the loaded
    /// version, the mapping drops its own (to be rebuilt if it is read
    /// again), so that what keeps the region alive is what calls hold.
    fn in_use(&self) -> bool {
        let mut held = self.held();
        if held.loaded.as_ref().is_some_and(|loaded| Arc::strong_count(loaded) > 1) {
            return true;
        }
        held.loaded = None;
        held.region.strong_count() > 0
    }
}

impl Attached {
    /// A mapping of `segment`, keeping what `held` holds.
    fn new(segment: Segment, held: Held) -> Self {
        Self { segment, bytes: segment.len(), held: Mutex::new(held) }
    }

    /// The loaded version this mapping holds, `version` with external ids
    /// `E`: built over the segment's region (and validated) the first time,
    /// then kept with the mapping, which it keeps in use while a call holds
    /// it.
    pub(crate) fn version<E: CachedIds>(
        &self,
        version: VersionId,
    ) -> Result<Arc<LoadedVersion<E>>, LoadError> {
        let mut held = self.held();
        if let Some(loaded) = &held.loaded {
            return Arc::clone(loaded)
                .downcast()
                .map_err(|_| broken("a shared version was read with another id type"));
        }
        let (loaded, region) = read::<E>(Box::new(self.segment.region()), version)?;
        let loaded = Arc::new(loaded);
        *held = Held { loaded: Some(loaded.clone()), region };
        Ok(loaded)
    }

    /// The persisted vector `record` describes, which this mapping holds:
    /// read in place the first time, then kept with the mapping, as
    /// [`Attached::version`] keeps a version.
    pub(crate) fn vector(&self, record: &VectorRecord) -> Result<Arc<AnyVector>, LoadError> {
        let mut held = self.held();
        if let Some(loaded) = &held.loaded {
            return Arc::clone(loaded)
                .downcast()
                .map_err(|_| broken("a shared vector was read as something else"));
        }
        let (vector, region) = read_vector(Box::new(self.segment.region()), record)?;
        let vector = Arc::new(vector);
        *held = Held { loaded: Some(vector.clone()), region };
        Ok(vector)
    }

    fn held(&self) -> MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Detaches the mapping, once nothing holds it or what was built over
    /// it (`Session::release` checks): the loaded version is dropped first.
    fn detach(self: Arc<Self>) {
        if let Ok(attached) = Arc::try_unwrap(self) {
            let Self { segment, held, .. } = attached;
            drop(held);
            segment.detach();
        }
    }
}

impl Loading {
    /// The region to fill.
    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: the loader fills it before it builds any view
        // (`validate` builds the first, after the fill).
        unsafe { self.region.bytes_mut() }
    }

    /// Reads the filled region as version `version`, validating it once,
    /// and keeps what it read for the published mapping.
    pub(crate) fn validate<E: CachedIds>(&mut self, version: VersionId) -> Result<(), LoadError> {
        let (loaded, region) = read::<E>(Box::new(self.region), version)?;
        let loaded: Arc<LoadedVersion<E>> = Arc::new(loaded);
        self.held = Held { loaded: Some(loaded), region };
        Ok(())
    }

    /// Reads the filled region as the persisted vector `record` describes,
    /// and keeps it for the published mapping.
    pub(crate) fn validate_vector(&mut self, record: &VectorRecord) -> Result<(), LoadError> {
        let (vector, region) = read_vector(Box::new(self.region), record)?;
        self.held = Held { loaded: Some(Arc::new(vector)), region };
        Ok(())
    }
}

#[cfg(test)]
mod release_tests {
    use super::release_due;

    #[test]
    fn a_release_is_due_after_a_drain_or_while_one_is_pending() {
        assert!(!release_due(3, 3, false), "nothing drained since the last release");
        assert!(release_due(4, 3, false), "a slot started draining since");
        assert!(release_due(3, 3, true), "a mapping of a draining slot was held then");
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests;
