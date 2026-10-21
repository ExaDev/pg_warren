//! Whole-graph compute: dense per-node vectors, semiring SpMV/SpMM kernels,
//! elementwise map/reduce, and `gather` over `Message`-generic buffers
//! (DESIGN.md §6.1–§6.4, §6.9).
//!
//! Every buffer a kernel allocates is charged to a [`ComputeMemory`]
//! budget, the domain side of the `warren.max_compute_memory` GUC.

mod cast;
mod dtype;
mod encoding;
mod error;
mod gather;
mod map;
mod mask;
pub(crate) mod memory;
mod message;
mod product;
mod ranking;
mod reduce;
#[cfg(test)]
#[cfg(not(miri))]
pub(crate) mod reference;
mod semiring;
mod summary;
mod symmetric;
mod triangles;
mod vector;
mod workers;

pub use dtype::{Dtype, Element};
pub use encoding::{encode, Decoder};
pub use error::{ArithmeticError, ComputeError};
pub use gather::{
    gather, Collected, Combine, Combiner, Gather, Gathered, Messages, Received, RowAt,
};
pub use map::{
    map, AnyExpr, Arithmetic, BoolOp, Comparands, Comparison, Expr, F32Op, F64Op, Function, I64Op,
    Value,
};
pub use mask::Mask;
pub use memory::{ComputeMemory, MemoryCharge, Reclaim};
pub use message::{Bytes, Message, MessageBuffer, MessageKind, PackedBytes};
pub use product::{spmm, spmv, Product};
pub use ranking::top_k;
pub use reduce::{norm_diff, reduce, reduce_live, Norm, Reduction};
pub use semiring::{BinaryOp, Monoid, Semiring, SemiringElement};
pub use summary::{summarise, Summary};
pub use triangles::triangles;
pub use vector::{AnyVector, Vector};
pub use workers::Workers;
