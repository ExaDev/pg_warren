//! Where a build sends each CSR as soon as it is complete, so that a caller
//! can write it out and free it before the next one is built.

use crate::graph::{AnyCsr, BuildCharge, GraphError};

/// Which CSR of a graph version: a relation's, or the merged CSR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsrScope {
    /// The CSR of the relation at this relation-dictionary index.
    Relation(usize),
    /// The merged (all-relations) CSR.
    Merged,
}

/// Out-edges or in-edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsrDirection {
    /// The forward CSR.
    Forward,
    /// The reverse CSR, built by transposing the forward one.
    Reverse,
}

/// A CSR a build has completed, with the build memory its buffers hold,
/// released when it is dropped.
#[derive(Debug)]
pub struct BuiltCsr {
    /// Whose CSR it is.
    pub scope: CsrScope,
    /// Its direction.
    pub direction: CsrDirection,
    /// The CSR.
    pub csr: AnyCsr,
    /// The charge for its buffers.
    pub charge: BuildCharge,
}

/// Takes the CSRs of a build in order: each relation's forward CSR then its
/// reverse CSR, in relation-index order, then the merged CSR's.
pub trait CsrSink {
    /// The sink's error, which a build error converts into.
    type Error: From<GraphError>;

    /// Takes one CSR.
    fn csr(&mut self, built: BuiltCsr) -> Result<(), Self::Error>;
}
