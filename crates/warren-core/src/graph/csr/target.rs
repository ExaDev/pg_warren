//! The width of a CSR's targets.

use std::fmt::Debug;

use crate::graph::DenseId;

mod sealed {
    pub trait Sealed {}
    impl Sealed for u32 {}
    impl Sealed for u64 {}
}

/// A target width: `u32` when the id space is below 2³², else `u64`.
pub trait Target: Copy + Ord + Debug + Send + Sync + 'static + sealed::Sealed {
    /// `id` at this width, if it fits.
    fn from_dense(id: DenseId) -> Option<Self>;

    /// This target as a dense id.
    fn dense(self) -> DenseId;
}

impl Target for u32 {
    fn from_dense(id: DenseId) -> Option<Self> {
        u32::try_from(id).ok()
    }

    fn dense(self) -> DenseId {
        DenseId::from(self)
    }
}

impl Target for u64 {
    fn from_dense(id: DenseId) -> Option<Self> {
        Some(id)
    }

    fn dense(self) -> DenseId {
        self
    }
}
