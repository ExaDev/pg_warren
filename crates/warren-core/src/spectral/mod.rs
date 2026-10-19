//! Spectral analysis and version distance: Lanczos eigenpairs of adjacency and
//! Laplacian operators, and structural/spectral distance between versions
//! (DESIGN.md §6.5, §8).

mod basis;
mod components;
mod dense;
mod distance;
mod error;
mod lanczos;
mod operator;
mod random;
mod rayleigh;
#[cfg(test)]
#[cfg(not(miri))]
mod reference;
mod resources;
mod ritz;
#[cfg(test)]
#[cfg(not(miri))]
mod tests;

pub use distance::{
    align, degree_distance, edit_distance, embedding_distance, spectral_distance,
    subspace_distance, ComparedRows, DegreeDistance, EdgeSetChange, EditDistance,
    EmbeddingDistance, RelationChange, SpectralDistance, SubspaceDistance,
};
pub use error::SpectralError;
pub use lanczos::{eigen, Eigen, EigenOptions, Eigenpair, Precision, Which};
pub use operator::{GraphMatrix, Operator};
pub use rayleigh::rayleigh_quotients;
pub use resources::Resources;
