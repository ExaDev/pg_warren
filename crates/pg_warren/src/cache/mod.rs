//! Backend-local cache (DESIGN.md §5.5): the loader, which decodes a
//! version's segments into its id map, relation dictionary and CSRs and
//! validates them; a backend-local map of loaded versions keyed by version
//! id, shared through `Arc`s; opening a version for a query (access check,
//! current version re-read per call, cache in front of the loader);
//! `warren.max_cache_memory`, the size bound that evicts cached versions
//! and vectors least recently used first (a pure policy in `lru`); and
//! `warren.prewarm` / `warren.evict`.
//!
//! Depends on `segment` (to read chunks), `catalog` (graphs and versions),
//! `access` (reader checks), `source` (id types) and `vector` (evict and
//! the size bound free cached vectors too, and the bound counts transient
//! vectors); `build`, `version_sql`, `traversal_sql` and
//! `compute_sql` use it.

mod bound;
mod error;
mod load;
mod lru;
mod open;
mod versions;
mod warm;

pub(crate) use bound::{admit_transient, define_max_cache_memory, trim};
pub(crate) use error::CacheError;
pub(crate) use load::{load, load_identity, LoadError, LoadedVersion};
pub(crate) use open::{cached, resolve, Resolved};
