//! Which nodes' rows a comparison of two versions' vectors reads.

/// The rows a vector comparison reads. A node is present in a set of
/// vectors when its row is non-zero in at least one of them: eigenvectors
/// are zero at tombstoned and isolated nodes, and stored vectors are zero
/// at tombstoned nodes (DESIGN.md §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeSet {
    /// Every node, a row absent from one side counting as zero
    /// (DESIGN.md §7). Nodes added or removed then count as change.
    All,
    /// Only the nodes present on both sides: how the vectors of the nodes
    /// the versions share changed, insensitive to added volume
    /// (Appendix A.2).
    Shared,
}
