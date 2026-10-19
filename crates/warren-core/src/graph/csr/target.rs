//! The width of a CSR's targets.

use std::fmt::Debug;

use crate::graph::DenseId;

mod sealed {
    pub trait Sealed {}
    impl Sealed for u32 {}
    impl Sealed for u64 {}
}

/// The target width of a graph version's CSRs: `u32` exactly when the id
/// space is below 2³² (DESIGN.md §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetWidth {
    /// 4-byte targets.
    U32,
    /// 8-byte targets.
    U64,
}

impl TargetWidth {
    /// The width every CSR over `id_space` uses.
    pub fn for_id_space(id_space: u64) -> Self {
        if id_space < 1 << 32 {
            Self::U32
        } else {
            Self::U64
        }
    }
}

/// A target type: `u32` or `u64` (see [`TargetWidth`]).
pub trait Target: Copy + Ord + Default + Debug + Send + Sync + 'static + sealed::Sealed {
    /// The width this type stores.
    const TARGET_WIDTH: TargetWidth;

    /// `id` at this width, if it fits.
    fn from_dense(id: DenseId) -> Option<Self>;

    /// This target as a dense id.
    fn dense(self) -> DenseId;
}

impl Target for u32 {
    const TARGET_WIDTH: TargetWidth = TargetWidth::U32;

    fn from_dense(id: DenseId) -> Option<Self> {
        u32::try_from(id).ok()
    }

    fn dense(self) -> DenseId {
        DenseId::from(self)
    }
}

impl Target for u64 {
    const TARGET_WIDTH: TargetWidth = TargetWidth::U64;

    fn from_dense(id: DenseId) -> Option<Self> {
        Some(id)
    }

    fn dense(self) -> DenseId {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_u32_exactly_when_the_id_space_is_below_2_pow_32() {
        assert_eq!(TargetWidth::for_id_space(0), TargetWidth::U32);
        assert_eq!(TargetWidth::for_id_space(u64::from(u32::MAX)), TargetWidth::U32);
        assert_eq!(TargetWidth::for_id_space(1 << 32), TargetWidth::U64);
        assert_eq!(TargetWidth::for_id_space(u64::MAX), TargetWidth::U64);
        assert_eq!(<u32 as Target>::TARGET_WIDTH, TargetWidth::U32);
        assert_eq!(<u64 as Target>::TARGET_WIDTH, TargetWidth::U64);
    }
}
