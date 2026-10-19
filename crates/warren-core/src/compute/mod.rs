//! Whole-graph compute: dense per-node vectors, semiring SpMV/SpMM kernels,
//! elementwise map/reduce, and `gather` over `Message`-generic buffers
//! (DESIGN.md §6.1–§6.4, §6.9).
//!
//! Every buffer a kernel allocates is charged to a [`ComputeMemory`]
//! budget, the domain side of the `warren.max_compute_memory` GUC.

mod dtype;
mod error;
mod memory;
mod product;
#[cfg(test)]
mod reference;
mod semiring;
mod vector;
mod workers;

pub use dtype::{Dtype, Element};
pub use error::{ArithmeticError, ComputeError};
pub use memory::{ComputeMemory, MemoryCharge};
pub use product::{spmm, spmv, Mask, Product};
pub use semiring::{BinaryOp, Monoid, Semiring, SemiringElement};
pub use vector::{AnyVector, Vector};
pub use workers::Workers;
