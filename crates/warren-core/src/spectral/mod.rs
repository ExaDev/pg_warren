//! Spectral analysis and version distance: Lanczos eigenpairs of adjacency and
//! Laplacian operators, and structural/spectral distance between versions
//! (DESIGN.md §6.5, §8).

mod basis;
mod dense;
mod error;
mod lanczos;
mod operator;
mod random;
#[cfg(test)]
#[cfg(not(miri))]
mod reference;
mod resources;
mod ritz;

pub use error::SpectralError;
pub use lanczos::{eigen, Eigen, EigenOptions, Eigenpair, Precision, Which};
pub use operator::{GraphMatrix, Operator};
pub use resources::Resources;
