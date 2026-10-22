//! The shared cache (DESIGN.md §5.6): one copy of a loaded version per
//! cluster, mapped by every backend that reads it. Owns the registry's
//! slot table and its state machine (claim, publish, abandon, pin, unpin,
//! attach, release, evict, drain), admission under
//! `warren.shared_cache_memory` and its fallbacks (a refusal means a
//! private load, never an error), a backend's own pins, mappings and
//! claims, the port `SharedVersions` that `cache::open` asks before its
//! backend-local cache, and the protocol that reads a version through it
//! (look up, wait, claim, load, publish). The `Memory` adapter is an
//! in-process fake whose backends are threads.

mod acquire;
mod admission;
mod memory;
mod model;
mod registry;
mod session;
mod slot;

use std::sync::Arc;

use admission::Refusal;
use registry::Gone;
use session::Release;
use slot::{Key, Ticket};

/// The shared cache as one backend sees it (DESIGN.md §5.6, §14 item 30).
/// `cache::open` asks it first and loads privately when it refuses.
pub(crate) trait SharedVersions {
    /// A region claimed to load a version into, until it is published or
    /// the load fails.
    type Region;
    /// A published region, as this backend maps it.
    type View;

    /// The version `key` names, pinned for the current statement if it is
    /// ready.
    fn lookup(&mut self, key: Key) -> Found<Self::View>;

    /// Claims a region of `bytes` to load `key`'s version into.
    fn claim(&mut self, key: Key, bytes: u64) -> Claimed<Self::Region>;

    /// Publishes a loaded region; the current statement pins it.
    fn publish(&mut self, region: Self::Region) -> Result<Arc<Self::View>, Gone>;

    /// Gives up what the current transaction was loading: the abort
    /// callback, after an ERROR unwound the load.
    fn abort(&mut self);

    /// Waits a while for another backend's load of the slot `loading`
    /// names, taking it over if its loader is gone.
    fn wait(&mut self, loading: Ticket);

    /// Ends the current statement's pins, without the registry's lock: it
    /// runs at the end of a transaction (DESIGN.md §5.6).
    fn unpin(&mut self);

    /// Releases the mappings `which` selects that no call holds.
    fn release(&mut self, which: Release);

    /// Evicts the unpinned versions whose keys `which` selects; how many.
    fn evict(&mut self, which: &dyn Fn(&Key) -> bool) -> usize;

    /// Gives back everything this backend holds, as it exits.
    fn exit(&mut self);
}

/// What a lookup finds.
#[derive(Debug)]
pub(crate) enum Found<V> {
    /// The version, mapped and pinned.
    Ready(Arc<V>),
    /// Another backend is loading it.
    Loading(Ticket),
    /// Nothing: claim it.
    Missing,
}

/// The outcome of a claim.
#[derive(Debug)]
pub(crate) enum Claimed<R> {
    /// Load it into this region, then publish it.
    Load(R),
    /// Another backend claimed it first: look again.
    Taken,
    /// Load it privately.
    Refused(Refusal),
}
