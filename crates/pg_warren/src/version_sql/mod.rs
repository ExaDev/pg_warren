//! SQL bindings for operations across versions of a graph (DESIGN.md §5.2,
//! §7): `warren.compact` and `warren.diff`. Delegates to
//! `warren_core::version` over versions loaded by `cache`; a compaction is
//! written like a build, through `segment` and `build`'s version catalog.
//!
//! Depends on `cache`, `build`, `catalog`, `segment` and `access`; nothing
//! depends on it.

mod compact;
mod diff;
mod error;
#[cfg(any(test, feature = "pg_test"))]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod merged_csr;
