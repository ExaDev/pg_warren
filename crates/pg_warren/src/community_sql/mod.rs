//! SQL bindings for communities (DESIGN.md §6.6): `warren.contract`, which
//! derives a quotient graph's version from a parent version and a label
//! vector, and `warren.project` / `warren.lift`, which move vectors between
//! a parent version and a quotient version contracted from it; and
//! (§6.7) `warren.modularity`, `warren.local_moving` and
//! `warren.leiden_refine` over a version's undirected weighted view.
//!
//! A contraction is a build: it writes its version through `build`'s tail,
//! within `warren.max_build_memory`, with the membership as one more
//! segment. Projection and lifting read and write vectors through
//! `compute_sql`'s `Vectors`, within `warren.max_compute_memory`. Depends
//! on `access`, `build`, `cache`, `catalog`, `compute_sql`, `interrupts`,
//! `segment` and `source`; nothing depends on it.

mod contract;
mod contraction;
mod error;
mod library;
mod local_moving;
mod modularity;
mod partition;
mod projection;

pub(crate) use error::CommunitySqlError;
