//! Which way along a graph's edges a traversal moves or a message travels
//! (DESIGN.md §6.4, §9.3).

/// Which way edges are followed from a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// From source to target: the forward CSR.
    Out,
    /// From target to source: the reverse CSR.
    In,
    /// Either way: the forward and reverse CSRs together.
    Both,
}

impl Direction {
    /// The opposite direction; `Both` is its own opposite.
    pub fn reversed(self) -> Self {
        match self {
            Self::Out => Self::In,
            Self::In => Self::Out,
            Self::Both => Self::Both,
        }
    }
}
