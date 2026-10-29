//! Spectral analysis and version distance: Lanczos eigenpairs of adjacency and
//! Laplacian operators, and structural/spectral distance between versions
//! (DESIGN.md §6.5, §8).

mod basis;
mod components;
mod deflation;
mod dense;
mod distance;
mod error;
mod lanczos;
mod operator;
mod rayleigh;
#[cfg(test)]
#[cfg(not(miri))]
mod reference;
mod resources;
mod ritz;
#[cfg(test)]
#[cfg(not(miri))]
mod tests;
mod working_set;

pub use distance::{
    align, align_ids, aligned_edit_distance, degree_distance, edit_distance, embedding_distance,
    spectral_distance, subspace_distance, ComparedRows, DegreeDistance, EdgeSetChange,
    EditDistance, EmbeddingDistance, IdAlignment, RelationChange, SpectralDistance,
    SubspaceDistance,
};
pub use error::SpectralError;
pub use lanczos::{admit_eigen, eigen, Eigen, EigenOptions, Eigenpair, Precision, Which};
pub use operator::{GraphMatrix, Operator};
pub use rayleigh::rayleigh_quotients;
pub use resources::Resources;
