//! SQL bindings for traversal (DESIGN.md §9.3): `warren.bfs`,
//! `warren.reachable_set`, `warren.neighbours`, `warren.degree`,
//! `warren.has_edge`, `warren.reachable`, `warren.shortest_path`,
//! `warren.dijkstra`, `warren.components`, `warren.walk` (with the
//! `warren.step` type, `warren.s` and the `jsonb` cast) and
//! `warren.last_stats`.
//!
//! Each call opens its version through `cache` (reader check, current
//! version re-read), maps external ids to dense ids, delegates to
//! `warren_core::traversal` with a main-thread interrupt poll, and maps the
//! result back. Depends on `cache`, `catalog`, `external_ids`,
//! `interrupts`, `selection` and `source`; `discard` forgets its last stats.

mod args;
mod bfs;
mod components;
mod error;
mod ids;
#[cfg(any(test, feature = "pg_test"))]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod merged_csr;
mod neighbourhood;
#[cfg(any(test, feature = "pg_test"))]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod oracle;
mod paths;
mod stats;
mod step;
#[cfg(any(test, feature = "pg_test"))]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod stored_ids;
mod walk;
mod walk_rows;

pub(crate) use error::{entry, TraversalSqlError};
pub(crate) use stats::forget_last_stats;
pub(crate) use walk_rows::define_max_walk_paths;
