//! SQL bindings for compute (DESIGN.md §6.2, §6.3, §6.9): named vectors
//! (`vector_from_query`, `vector_const`, `vector_degree`,
//! `vector_dense_id`, `vector_values`, `matrix_rows`, `vector_topk`,
//! `vector_stats`, `id_map`, `drop_vector`), semiring products (`spmv`,
//! `spmm`), `vector_map` with `explain_map`, `vector_reduce` and
//! `vector_norm_diff`, and the compute GUCs.
//!
//! Each call opens a version through `cache` (reader check, current version
//! re-read), reads and writes the vectors bound to it through `vector`,
//! and runs `warren_core::compute` kernels on the compute pool with a
//! main-thread interrupt poll, within a fresh `warren.max_compute_memory`
//! budget. Depends on `cache`, `catalog`, `vector`, `source` and the id
//! helpers of `traversal_sql`; nothing depends on it.

mod constructors;
mod drop;
mod error;
mod from_query;
mod read;
mod reduce;
mod settings;
mod summary;
mod vectors;

pub(crate) use error::{entry, ComputeSqlError};
pub(crate) use settings::define_compute_settings;
