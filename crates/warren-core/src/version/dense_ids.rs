//! Whether two versions of a graph share an id space: every dense id below
//! both id spaces names the same external id in each (DESIGN.md §5.2, §7).
//! Not true across a compaction, between graphs, or between sibling staged
//! versions built from one version that hand the same new dense ids to
//! different nodes. Relations play no part.

use super::VersionError;
use crate::graph::{ExternalId, IdMap};

/// `Ok` if every dense id of both `a` and `b` names the same external id
/// in each; else the first that does not.
pub fn shared_dense_ids<E: ExternalId>(a: &IdMap<E>, b: &IdMap<E>) -> Result<(), VersionError> {
    let shared = a.id_space().min(b.id_space());
    match (0..shared).find(|&dense| a.external_id(dense) != b.external_id(dense)) {
        Some(dense) => Err(VersionError::IdSpacesDiverge { dense }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::history::Build;

    #[test]
    fn a_version_and_its_successor_share_their_dense_ids_both_ways() {
        let v1 = Build::first(&[1, 2, 3], &[], false, false);
        let v2 = v1.next(&[1, 3, 4], &[], false, false);
        assert_eq!(shared_dense_ids(&v1.id_map, &v2.id_map), Ok(()));
        assert_eq!(shared_dense_ids(&v2.id_map, &v1.id_map), Ok(()));
    }

    #[test]
    fn sibling_staged_versions_give_one_new_dense_id_two_nodes() {
        // Both build from v1; 10 and 20 each get dense id 3.
        let v1 = Build::first(&[1, 2, 3], &[], false, false);
        let with_10 = v1.next(&[1, 2, 3, 10], &[], false, false);
        let with_20 = v1.next(&[1, 2, 3, 20], &[], false, false);
        assert_eq!(
            shared_dense_ids(&with_10.id_map, &with_20.id_map),
            Err(VersionError::IdSpacesDiverge { dense: 3 })
        );
    }
}
