//! Spectral analysis and version distance: Lanczos eigenpairs of adjacency and
//! Laplacian operators, and structural/spectral distance between versions
//! (DESIGN.md §6.5, §8).

mod basis;
mod dense;
mod distance;
mod error;
mod lanczos;
mod operator;
mod random;
#[cfg(test)]
#[cfg(not(miri))]
mod reference;
mod resources;
mod ritz;

pub use distance::{
    align, degree_distance, edit_distance, embedding_distance, spectral_distance,
    subspace_distance, DegreeDistance, EdgeSetChange, EditDistance, EmbeddingDistance,
    SpectralDistance, SubspaceDistance,
};
pub use error::SpectralError;
pub use lanczos::{eigen, Eigen, EigenOptions, Eigenpair, Precision, Which};
pub use operator::{GraphMatrix, Operator};
pub use resources::Resources;
