//! SQL bindings for spectral analysis and version distance (DESIGN.md
//! §6.5, §8): `warren.eigen`, which stores eigenvectors as named vectors,
//! and `warren.distance`, which compares two versions of a graph (or of two
//! graphs, aligned by external id) and returns `jsonb`.
//!
//! Each call opens its versions through `cache` (reader check, current
//! version re-read) and their vectors through `compute_sql`'s vector set,
//! and runs `warren_core::spectral` on the compute pool, within a fresh
//! `warren.max_compute_memory` budget and with a main-thread interrupt
//! poll. Depends on `access`, `cache`, `catalog`, `compute_sql`,
//! `interrupts`, `selection` and `source`; nothing depends on it.

mod caveats;
mod distance;
mod eigen;
mod error;
mod matrix;

pub(crate) use error::SpectralSqlError;
