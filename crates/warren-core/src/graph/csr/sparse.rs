//! When a CSR's offsets are sparse (DESIGN.md §5.3, §14 item 17): a
//! relation's CSR stores sparse offsets, its rows' ids and one offset per
//! stored row, when they take at most [`SPARSE_SHARE`] of dense offsets'
//! bytes; the merged CSR's offsets are always dense. The choice is made
//! from the number of stored rows alone, so a CSR's layout is a function
//! of its edges.

use super::TargetWidth;

/// Sparse offsets are stored when they take at most this share of the
/// bytes of dense ones, so where a relation's rows fill at most about a
/// sixth of the id space (an eighth at `u64` targets): past that a row
/// lookup's binary search costs more than the few bytes it saves are worth
/// (measured in DESIGN.md §5.3).
const SPARSE_SHARE: (u64, u64) = (1, 4);

/// The bytes of sparse offsets for `stored_rows` rows: a row id of the
/// target width and an offset per row, and the end offset.
fn sparse_bytes(stored_rows: u64, width: TargetWidth) -> u64 {
    let id = match width {
        TargetWidth::U32 => 4,
        TargetWidth::U64 => 8,
    };
    stored_rows.saturating_mul(id + 8).saturating_add(8)
}

/// The bytes of dense offsets over `id_space`.
pub(crate) fn dense_bytes(id_space: u64) -> u64 {
    id_space.saturating_add(1).saturating_mul(8)
}

/// Whether a relation's CSR over `id_space` with `stored_rows` non-empty
/// rows stores sparse offsets.
pub(crate) fn stores_sparse(stored_rows: u64, id_space: u64, width: TargetWidth) -> bool {
    let (share, of) = SPARSE_SHARE;
    sparse_bytes(stored_rows, width).saturating_mul(of)
        <= dense_bytes(id_space).saturating_mul(share)
}

/// The bytes of a relation CSR's offsets (and stored rows, if sparse)
/// with `stored_rows` non-empty rows: the layout [`stores_sparse`] picks.
/// It never decreases as rows are added, so at most `edges` rows bound a
/// CSR of `edges` edges.
fn offsets_bytes(stored_rows: u64, id_space: u64, width: TargetWidth) -> u64 {
    match stores_sparse(stored_rows, id_space, width) {
        true => sparse_bytes(stored_rows, width),
        false => dense_bytes(id_space),
    }
}

/// The most bytes of the offsets (and stored rows) of a relation's CSR of
/// `edges` edges over `id_space`, at that id space's target width: its
/// stored rows are at most its edges and the id space.
pub(crate) fn offsets_bound(edges: u64, id_space: u64) -> u64 {
    offsets_bytes(edges.min(id_space), id_space, TargetWidth::for_id_space(id_space))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_are_sparse_where_they_take_at_most_a_quarter_of_the_bytes_of_dense_ones() {
        // 1,000 nodes: dense offsets take 8,008 bytes; sparse ones 12 per
        // row at u32, 16 at u64, and 8.
        assert!(stores_sparse(0, 1000, TargetWidth::U32));
        assert!(stores_sparse(166, 1000, TargetWidth::U32));
        assert!(!stores_sparse(167, 1000, TargetWidth::U32));
        assert!(stores_sparse(124, 1000, TargetWidth::U64));
        assert!(!stores_sparse(125, 1000, TargetWidth::U64));
        assert_eq!(offsets_bytes(2, 1000, TargetWidth::U32), 2 * 12 + 8);
        assert_eq!(offsets_bytes(167, 1000, TargetWidth::U32), 8008);
        // An empty id space's one offset is the same either way.
        assert_eq!(offsets_bytes(0, 0, TargetWidth::U32), 8);
    }

    #[test]
    fn a_relation_of_few_edges_is_bounded_by_its_edges_and_any_by_dense_offsets() {
        assert_eq!(offsets_bound(2, 1_000_000), 2 * 12 + 8);
        assert_eq!(offsets_bound(0, 1_000_000), 8);
        assert_eq!(offsets_bound(5_000_000, 1_000_000), dense_bytes(1_000_000));
        assert_eq!(offsets_bound(2, 1 << 33), 2 * 16 + 8);
    }

    #[test]
    fn the_offsets_bytes_never_decrease_with_the_stored_rows() {
        for width in [TargetWidth::U32, TargetWidth::U64] {
            for id_space in [0, 1, 7, 100, 1000] {
                let bytes: Vec<u64> =
                    (0..=id_space).map(|rows| offsets_bytes(rows, id_space, width)).collect();
                assert!(bytes.windows(2).all(|pair| pair[0] <= pair[1]), "{id_space}: {bytes:?}");
                assert!(bytes.iter().all(|&b| b <= dense_bytes(id_space)));
            }
        }
    }
}

/// Every reader of relations' CSRs returns with sparse offsets exactly
/// what it returns with dense ones.
#[cfg(test)]
mod readers {
    use proptest::prelude::*;

    use crate::community::{contract, Contraction};
    use crate::compute::{
        gather, spmv, triangles, AnyVector, Combiner, ComputeMemory, Gather, Messages, Product,
        Semiring, Vector, Workers,
    };
    use crate::graph::{
        CsrLayout, DenseId, Direction, EdgeList, GraphCsr, Membership, RelationDictionary,
        RelationIndex, RelationSelection,
    };
    use crate::interrupts::Uninterrupted;
    use crate::traversal::{bfs, components, dijkstra, Adjacency, Limits};

    /// A graph of up to 30 relations over up to 80 nodes, whose relations
    /// are mostly sparse, built (so with sparse offsets where they pay).
    fn graphs() -> impl Strategy<Value = (GraphCsr, Vec<RelationIndex>)> {
        (1usize..30, 8u64..80, any::<bool>(), any::<bool>()).prop_flat_map(
            |(relation_count, id_space, store_merged, build_reverse)| {
                let edge = (0..relation_count, 0..id_space, 0..id_space, 1u8..9);
                prop::collection::vec(edge, 0..200).prop_map(move |rows| {
                    let mut dictionary = RelationDictionary::new();
                    let relations: Vec<RelationIndex> = (0..relation_count)
                        .map(|r| dictionary.intern(&r.to_string()).unwrap())
                        .collect();
                    let mut edges = EdgeList::new(true);
                    for &(r, source, target, weight) in &rows {
                        edges.push(relations[r], source, target, f64::from(weight) / 4.0).unwrap();
                    }
                    let layout = CsrLayout { build_reverse, store_merged };
                    let graph =
                        GraphCsr::build_as(edges, relation_count, id_space, layout).unwrap();
                    (graph, relations)
                })
            },
        )
    }

    /// Every result the readers give for `graph`, as text.
    fn read(graph: &GraphCsr, relations: &[RelationIndex]) -> Vec<String> {
        let id_space = graph.all_relations().forward().id_space();
        let (workers, memory) = (Workers::new(2).unwrap(), ComputeMemory::new(u64::MAX));
        let mut never = Uninterrupted;
        let some: Vec<RelationIndex> = relations.iter().copied().step_by(2).collect();
        let selections = [RelationSelection::All, RelationSelection::Only(&some)];
        let mut results = Vec::new();
        for selection in selections {
            for direction in [Direction::Out, Direction::In, Direction::Both] {
                let Ok(adjacency) = Adjacency::new(graph, direction, selection) else { continue };
                let visits = bfs(&adjacency, &[0], None, Limits::default(), &mut never);
                results.push(format!("bfs {direction:?} {visits:?}"));
                let path = dijkstra(&adjacency, 0, id_space - 1, &mut never);
                results.push(format!("dijkstra {direction:?} {path:?}"));
            }
            let values = (0..id_space).map(|node| node as f64 + 0.5).collect();
            let x = AnyVector::from(Vector::from_values(&memory, id_space, 1, values).unwrap());
            for transpose in [false, true] {
                let product = Product {
                    transpose,
                    relations: selection,
                    ..Product::new(Semiring::PlusTimes)
                };
                let mut y = AnyVector::from(Vector::filled(&memory, id_space, 1, 0.0f64).unwrap());
                let done = spmv(&workers, &memory, &mut never, graph, &product, &x, &mut y);
                results.push(format!("spmv {transpose} {done:?} {y:?}"));
            }
            let mut messages = Messages::<f64>::new(&memory).unwrap();
            (0..id_space).for_each(|node| messages.push(node, node as f64).unwrap());
            for direction in [Direction::Out, Direction::In, Direction::Both] {
                let sum = Gather { direction, relations: selection, ..Gather::new(Combiner::Sum) };
                let gathered =
                    gather(&workers, &memory, &mut never, graph, &sum, &messages).map(|gathered| {
                        gathered.iter().map(|row| format!("{row:?}")).collect::<Vec<_>>()
                    });
                results.push(format!("gather {direction:?} {gathered:?}"));
            }
            let counted = triangles(&workers, &memory, &mut never, graph, selection);
            results.push(format!("triangles {counted:?}"));
        }
        results.push(format!("components {:?}", components(graph, &mut never)));
        let thirds: Vec<Option<DenseId>> = (0..id_space).map(|node| Some(node % 3)).collect();
        let membership = Membership::new(&thirds, 3).unwrap();
        let forward: Vec<_> = relations
            .iter()
            .zip(graph.relations())
            .map(|(&relation, csrs)| (relation, csrs.forward()))
            .collect();
        let contraction = Contraction { weighted: true, drop_self_loops: false, undirected: false };
        let memory = crate::graph::BuildMemory::unlimited();
        let quotient = contract(&workers, &mut never, &forward, &membership, contraction, &memory);
        results.push(format!("contract {quotient:?}"));
        results
    }

    proptest! {
        #[test]
        fn readers_return_with_sparse_offsets_what_they_return_with_dense_ones(
            (graph, relations) in graphs(),
        ) {
            let dense = graph.densified();
            let (sparse, dense) = (read(&graph, &relations), read(&dense, &relations));
            for (sparse, dense) in sparse.iter().zip(&dense) {
                prop_assert_eq!(sparse, dense);
            }
            prop_assert_eq!(sparse.len(), dense.len());
        }
    }
}
