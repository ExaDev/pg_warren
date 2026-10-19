//! Backend-local cache (DESIGN.md §5.5): the loader, which decodes a
//! version's segments into its id map, relation dictionary and CSRs and
//! validates them; a backend-local map of loaded versions keyed by version
//! id, shared through `Arc`s; opening a version for a query (access check,
//! current version re-read per call, cache in front of the loader); and
//! `warren.prewarm` / `warren.evict`.
//!
//! Depends on `segment` (to read chunks), `catalog` (graphs and versions),
//! `access` (reader checks) and `source` (id types); `build`,
//! `version_sql` and `traversal_sql` use it.

mod error;
mod load;
mod open;
mod versions;
mod warm;

pub(crate) use error::CacheError;
pub(crate) use load::{load, load_identity, LoadError, LoadedVersion};
pub(crate) use open::{cached, resolve, Resolved};
