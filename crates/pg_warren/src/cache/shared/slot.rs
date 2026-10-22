//! A slot of the registry: which version it holds (its key), in which
//! state, and who uses it.

use pgrx::pg_sys::Oid;

use crate::catalog::VersionId;

/// A backend's process id.
pub(crate) type Pid = i32;

/// What a slot holds: a version of one installation of pg_warren in one
/// database. `version_id` restarts after `DROP EXTENSION` and `CREATE
/// EXTENSION`, whose extension oid differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    /// The database's oid.
    pub(crate) database: Oid,
    /// pg_warren's `pg_extension` oid in that database.
    pub(crate) extension: Oid,
    /// The version.
    pub(crate) version: VersionId,
}

impl Key {
    /// The key of an empty slot, which no version has.
    const NONE: Self = Self { database: Oid::INVALID, extension: Oid::INVALID, version: 0 };
}

/// Where a slot is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    /// Free.
    Empty,
    /// A backend is loading its version into a new region.
    Loading,
    /// Its version is loaded, published and readable.
    Ready,
    /// Evicted: no new reader may use it; it is empty once its last
    /// mapper releases it.
    Draining,
}

/// A published region's handle, by which backends attach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Handle(pub(crate) u32);

/// One use of a slot: its index and the generation its claim stamped, so
/// that an operation on a slot since emptied and claimed again does
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Ticket {
    /// The slot's index.
    pub(crate) slot: usize,
    /// The generation of the claim that filled it.
    pub(crate) generation: u64,
}

/// A slot of the registry. Only the registry changes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    pub(super) state: State,
    pub(super) key: Key,
    /// The region's handle, once published.
    pub(super) handle: Handle,
    /// The region's size.
    pub(super) bytes: u64,
    /// The registry's clock when a backend last pinned it.
    pub(super) last_used: u64,
    /// Backends whose current statement pins it.
    pub(super) pins: u32,
    /// Backends that map its region.
    pub(super) mappers: u32,
    /// The backend that claimed it to load.
    pub(super) loader: Pid,
    /// The generation of the claim that filled it.
    pub(super) generation: u64,
}

impl Slot {
    /// A free slot.
    pub(crate) const EMPTY: Self = Self {
        state: State::Empty,
        key: Key::NONE,
        handle: Handle(0),
        bytes: 0,
        last_used: 0,
        pins: 0,
        mappers: 0,
        loader: 0,
        generation: 0,
    };

    /// Whether it may be evicted: ready and pinned by no statement.
    pub(super) fn evictable(&self) -> bool {
        self.state == State::Ready && self.pins == 0
    }

    /// Whether evicting it frees its bytes and the slot at once: evictable
    /// and mapped by no backend.
    pub(super) fn reclaimable(&self) -> bool {
        self.evictable() && self.mappers == 0
    }

    /// Whether it counts against the cap: loading, ready or draining.
    pub(super) fn occupied(&self) -> bool {
        self.state != State::Empty
    }
}
