//! Test: a graph keyed by text ids behaves, version by version, as the same
//! graph keyed by `int8` under the order-preserving bijection from each
//! text id to its rank (DESIGN.md §5.7): rebuilds that add, drop and bring
//! back nodes, `diff`, the shared-dense-id check, and compaction in either
//! node order, which renumbers by locality.

use proptest::prelude::*;

use super::history::{builds, ranks, Build, Ranks};
use super::{compact, diff, shared_dense_ids};
use crate::graph::{BuildMemory, FixedKeys, IdMap, NodeOrder, TextKeys};

/// `text` is `int` with each rank replaced by its text id.
fn same_ids(
    text: &IdMap<TextKeys>,
    int: &IdMap<FixedKeys<i64>>,
    ranks: &Ranks,
) -> Result<(), TestCaseError> {
    prop_assert_eq!(text.id_space(), int.id_space());
    prop_assert_eq!(text.tombstones(), int.tombstones());
    for dense in 0..int.id_space() {
        let rank = *int.external_id(dense).unwrap();
        let id = ranks.0[rank as usize].as_slice();
        prop_assert_eq!(text.external_id(dense), Some(id));
        prop_assert_eq!(text.dense_id(id), int.dense_id(&rank));
    }
    Ok(())
}

proptest! {
    #[test]
    fn text_keyed_versions_behave_as_int_keyed_ones(
        ranks in ranks(),
        history in builds(4),
        weighted in any::<bool>(),
        reverse in any::<bool>(),
    ) {
        let mut int = Build::first(&[], &[], weighted, reverse);
        let mut text = Build::first_with(&ranks, &[], &[], weighted, reverse);
        for (nodes, edges) in &history {
            let int_next = int.next(nodes, edges, weighted, reverse);
            let text_next = text.next_with(&ranks, nodes, edges, weighted, reverse);
            same_ids(&text_next.id_map, &int_next.id_map, &ranks)?;
            prop_assert_eq!(&text_next.relations, &int_next.relations);
            prop_assert_eq!(&text_next.graph, &int_next.graph);
            prop_assert_eq!(
                diff(&text.version(), &text_next.version()),
                diff(&int.version(), &int_next.version())
            );
            // A sibling of `text_next`, built from the same base.
            let sibling = text.next_with(&ranks, &nodes[nodes.len() / 2..], &[], weighted, reverse);
            let int_sibling = int.next(&nodes[nodes.len() / 2..], &[], weighted, reverse);
            prop_assert_eq!(
                shared_dense_ids(&text_next.id_map, &sibling.id_map),
                shared_dense_ids(&int_next.id_map, &int_sibling.id_map)
            );
            for order in [NodeOrder::Id, NodeOrder::Locality] {
                let memory = BuildMemory::unlimited();
                let compacted = compact(&text_next.version(), order, &memory).unwrap();
                let expected = compact(&int_next.version(), order, &memory).unwrap();
                same_ids(&compacted.id_map, &expected.id_map, &ranks)?;
                prop_assert_eq!(&compacted.graph, &expected.graph);
                prop_assert_eq!(&compacted.renumbering, &expected.renumbering);
            }
            (int, text) = (int_next, text_next);
        }
    }
}
