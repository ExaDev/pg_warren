//! Backend-local cache (DESIGN.md §5.5): the loader, which copies a
//! version's segments into one 64-byte-aligned region (`region`, §5.6)
//! and reads its id map, relation dictionary and CSRs through shared views
//! of it, validating them; a backend-local map of loaded versions keyed by version
//! id, shared through `Arc`s; opening a version for a query (access check,
//! current version re-read per call, cache in front of the loader);
//! `warren.max_cache_memory`, the size bound that evicts cached versions
//! and vectors least recently used first (a pure policy in `lru`), with
//! `warren.max_compute_memory`, which together bound a backend's peak; the
//! statement pin, which keeps what a top-level statement has read cached
//! until it ends; `warren.prewarm` / `warren.evict`; and the shared cache
//! (`shared`, §5.6), one copy of a version per cluster.
//!
//! Depends on `segment` (to read chunks), `catalog` (graphs and versions),
//! `access` (reader checks), `source` (id types) and `vector` (evict and
//! the size bound free cached vectors too, and the bound counts transient
//! vectors); `build`, `version_sql`, `traversal_sql` and
//! `compute_sql` use it.

mod bound;
mod error;
mod installation;
mod load;
mod lru;
mod open;
mod region;
mod shared;
mod statement;
mod versions;
mod warm;

pub(crate) use bound::{admit_transient, compute_memory, define_memory_settings, trim};
pub(crate) use error::CacheError;
pub(crate) use load::{load, load_identity, Identity, LoadError, LoadedVersion};
pub(crate) use open::{
    cached, cached_pair, cached_vector, cached_vector_hit, keep_vector, resolve, Resolved,
};
pub(crate) use shared::define_shared_cache;
#[cfg(any(test, feature = "pg_test"))]
pub(crate) use statement::end_statement;
