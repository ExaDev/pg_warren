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
//! result back. Depends on `cache`, `catalog`, `interrupts` and `source`;
//! `compute_sql` reuses its id and argument helpers.

mod args;
mod bfs;
mod components;
mod error;
mod ids;
mod neighbourhood;
#[cfg(any(test, feature = "pg_test"))]
mod oracle;
mod paths;
mod stats;
mod step;
mod walk;
mod walk_rows;

pub(crate) use args::{direction, relations};
pub(crate) use error::{entry, TraversalSqlError};
pub(crate) use ids::{check_type, external};
pub(crate) use walk_rows::define_max_walk_paths;
