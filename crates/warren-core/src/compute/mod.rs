//! Whole-graph compute: dense per-node vectors, semiring SpMV/SpMM kernels,
//! elementwise map/reduce, and `gather` over `Message`-generic buffers
//! (DESIGN.md §6.1–§6.4, §6.9).
//!
//! Every buffer a kernel allocates is charged to a [`ComputeMemory`]
//! budget, the domain side of the `warren.max_compute_memory` GUC.

mod cast;
mod centrality;
mod core_number;
mod dag;
mod dtype;
mod encoding;
mod error;
mod gather;
mod map;
mod mask;
pub(crate) mod memory;
mod message;
mod product;
pub(crate) mod random;
mod ranking;
mod reduce;
#[cfg(test)]
#[cfg(not(miri))]
pub(crate) mod reference;
mod semiring;
mod similarity;
mod spanning_forest;
mod strong_components;
mod summary;
mod symmetric;
pub(crate) mod tally;
mod triangles;
mod vector;
mod workers;

pub use centrality::{
    betweenness_centrality, closeness_centrality, harmonic_centrality, BetweennessScale,
    Centrality, Sampling, ShortestPaths,
};
pub use core_number::{core_number, CoreNumbers};
pub use dag::{dag_longest_path, topological_order, LongestPaths};
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
pub use similarity::{
    most_similar, similar_pairs, similarity_of_pairs, Neighbourhood, Scores, Similar, SimilarPairs,
    Similarity,
};
pub use spanning_forest::{minimum_spanning_forest, ForestEdge, SpanningForest};
pub use strong_components::strong_components;
pub use summary::{summarise, Summary};
pub use triangles::triangles;
pub use vector::{AnyVector, Vector};
pub use workers::Workers;
