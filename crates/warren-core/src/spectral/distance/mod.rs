//! Distances between two versions of a graph (DESIGN.md §8,
//! `warren.distance`), each summarised by a few scalars (§14 item 11).
//!
//! Versions of a graph share its id space (DESIGN.md §5.2, §7), so their
//! vectors and CSRs are aligned by dense id: a later version's id space may
//! be larger, and rows past a version's id space, like tombstoned rows,
//! count as absent (no edges, zero values). Vectors of another graph are
//! brought into the id space first with [`align`].

mod alignment;
mod degree;
mod edit;
mod eigenvalues;
mod embedding;
mod node_set;
mod rows;
mod subspace;

pub use alignment::align;
pub use degree::{degree_distance, DegreeDistance};
pub use edit::{edit_distance, EdgeSetChange, EditDistance};
pub use eigenvalues::{spectral_distance, SpectralDistance};
pub use embedding::{embedding_distance, EmbeddingDistance};
pub use node_set::NodeSet;
pub use subspace::{subspace_distance, SubspaceDistance};
