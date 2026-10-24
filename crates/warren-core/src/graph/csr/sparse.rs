//! When a CSR's offsets are sparse (DESIGN.md §5.3, §14 item 17): a
//! relation's CSR stores sparse offsets, its rows' ids and one offset per
//! stored row, when they take at most [`SPARSE_SHARE`] of dense offsets'
//! bytes; the merged CSR's offsets are always dense. The choice is made
//! from the number of stored rows alone, so a CSR's layout is a function
//! of its edges.

use super::TargetWidth;

/// Sparse offsets are stored when they take at most this share of the
/// bytes of dense ones: below it a row lookup's binary search costs more
/// than the memory saved is worth (measured in DESIGN.md §5.3).
const SPARSE_SHARE: (u64, u64) = (1, 2);

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
    fn offsets_are_sparse_where_they_take_at_most_half_the_bytes_of_dense_ones() {
        // 1,000 nodes: dense offsets take 8,008 bytes; sparse ones 12 per
        // row at u32, 16 at u64, and 8.
        assert!(stores_sparse(0, 1000, TargetWidth::U32));
        assert!(stores_sparse(333, 1000, TargetWidth::U32));
        assert!(!stores_sparse(334, 1000, TargetWidth::U32));
        assert!(stores_sparse(249, 1000, TargetWidth::U64));
        assert!(!stores_sparse(250, 1000, TargetWidth::U64));
        assert_eq!(offsets_bytes(2, 1000, TargetWidth::U32), 2 * 12 + 8);
        assert_eq!(offsets_bytes(334, 1000, TargetWidth::U32), 8008);
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
