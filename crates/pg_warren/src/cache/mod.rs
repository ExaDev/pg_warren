//! Backend-local cache (DESIGN.md §5.5). For now, its loader: decoding a
//! version's segments into its id map, relation dictionary and CSRs,
//! validated before use. M3 adds the backend-local map keyed by version id,
//! `prewarm` and `evict` on top.
//!
//! Depends on `segment` (to read chunks) and `catalog` (version ids);
//! `build` and `version_sql` use it.

mod load;

pub(crate) use load::{load, load_identity, LoadError};
