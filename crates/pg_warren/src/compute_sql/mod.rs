//! SQL bindings for compute: vectors, `spmv`, `vector_map`, `gather`, `eigen`,
//! `distance` (DESIGN.md §6, §8). Delegates kernels to `warren_core::compute`
//! and `warren_core::spectral`. Arrives in M4–M6.
