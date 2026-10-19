//! SQL bindings for traversal (DESIGN.md §9.3): `warren.bfs`,
//! `warren.reachable_set`, `warren.neighbours`, `warren.degree`,
//! `warren.has_edge`, `warren.reachable`, `warren.shortest_path`,
//! `warren.dijkstra`, `warren.components` and `warren.last_stats`.
//!
//! Each call opens its version through `cache` (reader check, current
//! version re-read), maps external ids to dense ids, delegates to
//! `warren_core::traversal` with a main-thread interrupt poll, and maps the
//! result back. Depends on `cache`, `catalog` and `source`; nothing depends
//! on it.

mod args;
mod bfs;
mod components;
mod error;
mod ids;
mod interrupts;
mod neighbourhood;
mod oracle;
mod paths;
mod stats;

pub(crate) use error::{entry, TraversalSqlError};
