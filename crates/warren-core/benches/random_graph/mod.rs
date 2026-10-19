//! A fixed pseudo-random edge list shared by the benchmarks, so runs are
//! comparable.

use warren_core::graph::{EdgeList, GraphError, RelationDictionary};

/// Dense ids in the benchmark graphs.
pub const ID_SPACE: u64 = 100_000;
/// Edges in the benchmark graphs.
pub const EDGES: u64 = 1_000_000;

/// [`EDGES`] uniformly random edges over [`ID_SPACE`] (64-bit LCG), all
/// of weight 1 and in one relation.
pub fn random_edges(weighted: bool) -> Result<EdgeList, GraphError> {
    let relation = RelationDictionary::new().intern("edge")?;
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state =
            state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        state >> 33
    };
    let mut edges = EdgeList::for_id_space(ID_SPACE, weighted);
    for _ in 0..EDGES {
        let (source, target) = (next() % ID_SPACE, next() % ID_SPACE);
        edges.push(relation, source, target, 1.0)?;
    }
    Ok(edges)
}
