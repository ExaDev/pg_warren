//! SQL bindings for compute (DESIGN.md §6.2–§6.4, §6.7, §6.9): named
//! vectors (`vector_from_query`, `vector_const`, `vector_degree`,
//! `vector_dense_id`, `triangles`, `vector_values`, `matrix_rows`,
//! `vector_topk`, `vector_stats`, `id_map`, `drop_vector`), semiring
//! products (`spmv`, `spmm`), `vector_map` with `explain_map`,
//! `vector_reduce` and `vector_norm_diff`, `gather`, the PL/pgSQL
//! algorithm library, and the compute GUCs.
//!
//! Each call opens a version through `cache` (reader check, current version
//! re-read), reads and writes the vectors bound to it through `vector`,
//! and runs `warren_core::compute` kernels on the compute pool with a
//! main-thread interrupt poll, within a fresh `warren.max_compute_memory`
//! budget. Depends on `access`, `cache`, `catalog`, `external_ids`,
//! `interrupts`, `selection`, `source` and `vector`; `spectral_sql` uses its
//! vectors, compute pool and memory budget.

mod constructors;
mod drop;
mod error;
mod from_query;
mod gather;
mod library;
#[cfg(any(test, feature = "pg_test"))]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod limits;
mod map;
#[cfg(any(test, feature = "pg_test"))]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod oracle;
#[cfg(any(test, feature = "pg_test"))]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod ownership;
mod product;
mod read;
mod reduce;
mod settings;
mod summary;
#[cfg(any(test, feature = "pg_test"))]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod transient;
mod vectors;

pub(crate) use drop::drop_named;
pub(crate) use error::{compute_sqlstate, entry, ComputeSqlError};
pub(crate) use settings::{compute_memory, define_compute_settings, workers};
pub(crate) use vectors::Vectors;
