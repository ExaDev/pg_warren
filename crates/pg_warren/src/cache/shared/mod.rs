//! The shared cache (DESIGN.md §5.6): one copy of a loaded version, or of
//! a persisted vector (§14 item 38), per cluster, mapped by every backend
//! that reads it; a slot's key says which kind it holds. Owns the registry's
//! slot table and its state machine (claim, publish, abandon, pin, unpin,
//! attach, release, evict, drain), admission under
//! `warren.shared_cache_memory` and its fallbacks (a refusal means a
//! private load, never an error), a backend's own pins, mappings and
//! claims, the port `SharedVersions` that `cache::open` asks before its
//! backend-local cache, and the protocol that reads a version through it
//! (look up, wait, claim, load, publish). Three adapters: `Dsm`, the
//! registry and a DSM segment per version in shared memory; `Unavailable`,
//! where it cannot work (PG 15 and 16 without the preload, a big-endian
//! target), which loads privately; and `Memory`, an in-process fake whose
//! backends are threads. `read` is how `cache::open` asks it first;
//! `setting` is `warren.shared_cache_memory`; `introspection` is
//! `warren.shared_cache()`, what the registry holds.

mod acquire;
mod admission;
mod dsm;
mod eviction;
mod introspection;
#[cfg(test)]
mod memory;
#[cfg(test)]
mod model;
mod read;
mod registry;
mod session;
mod setting;
mod slot;
mod unavailable;

pub(crate) use read::{evict_graph, evict_vector, prune, shared, shared_hit, shared_vector};

/// Registers `warren.shared_cache_memory` and, while
/// `shared_preload_libraries` loads pg_warren on PG 15 and 16, the hooks
/// that place the registry in shared memory. Called once, from `_PG_init`.
pub(crate) fn define_shared_cache() {
    setting::define();
    dsm::request_preload();
}

/// Ends the current statement's pins of shared versions: pure Rust,
/// without the registry's lock, so it may run at a transaction's end
/// (DESIGN.md §5.5, §5.6).
pub(crate) fn end_statement() {
    dsm::Dsm::here().unpin();
}

/// At the end of a client message, releases the mappings of evicted slots
/// that no call holds (DESIGN.md §14 item 28). It takes the registry's
/// lock and detaches segments, so never at a transaction's end.
pub(crate) fn end_message() {
    dsm::Dsm::here().release();
}

use std::sync::Arc;

use admission::Refusal;
use registry::{Gone, Listing};
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

    /// Releases the mappings of evicted slots that no call holds; those of
    /// ready slots stay (DESIGN.md §14 item 28).
    fn release(&mut self);

    /// Evicts the unpinned versions whose keys `which` selects; how many.
    /// (A read with `warren.shared_cache_memory` at 0, `warren.evict`, a
    /// prune and the load of a graph's current version, `eviction`.)
    fn evict(&mut self, which: &dyn Fn(&Key) -> bool) -> usize;

    /// Releases this backend's mappings of the versions whose keys `which`
    /// selects, ready or evicted, that no call holds.
    fn forget(&mut self, which: &dyn Fn(&Key) -> bool);

    /// The keys of the versions the registry holds, in any state.
    fn keys(&mut self) -> Vec<Key>;

    /// The slots in use and the registry's clock (`warren.shared_cache()`).
    fn listing(&mut self) -> Listing;

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
