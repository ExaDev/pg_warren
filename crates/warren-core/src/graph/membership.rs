//! The membership of a contraction (DESIGN.md §6.6): which community each
//! node of the parent version belongs to, as a dense id of the quotient
//! version, or none for a node that belongs to no community (a tombstoned
//! one). It is the matrix P (N × K, one entry per row of a live node),
//! stored as the quotient version's `membership` segment.

use super::{memory, DenseId, GraphError, Tombstones};

/// Each parent node's community: a dense id of the quotient version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    communities: Vec<u64>,
    quotient_id_space: u64,
}

/// The stored community of a node that belongs to none.
pub(crate) const NO_COMMUNITY: u64 = u64::MAX;

impl Membership {
    /// The membership whose node `n` belongs to `communities[n]` (`None`:
    /// to no community), each a dense id below `quotient_id_space`; for
    /// tests (a build makes one from labels, charged to its memory).
    #[cfg(test)]
    pub(crate) fn new(
        communities: &[Option<DenseId>],
        quotient_id_space: u64,
    ) -> Result<Self, GraphError> {
        let mut stored = memory::with_capacity(communities.len())?;
        stored.extend(communities.iter().map(|c| c.unwrap_or(NO_COMMUNITY)));
        Self::from_stored(stored, quotient_id_space)
    }

    /// The membership whose node `n` belongs to `stored[n]`, or to none if
    /// that is [`NO_COMMUNITY`], checking every community lies below
    /// `quotient_id_space`.
    pub(crate) fn from_stored(
        stored: Vec<u64>,
        quotient_id_space: u64,
    ) -> Result<Self, GraphError> {
        let outside = stored.iter().position(|&c| c != NO_COMMUNITY && c >= quotient_id_space);
        if let Some(node) = outside {
            let community = stored[node];
            return Err(GraphError::CommunityOutsideQuotient { node: node as u64, community });
        }
        Ok(Self { communities: stored, quotient_id_space })
    }

    /// The parent version's id space: one entry per parent node.
    pub fn id_space(&self) -> u64 {
        self.communities.len() as u64
    }

    /// The quotient version's id space: the communities are below it.
    pub fn quotient_id_space(&self) -> u64 {
        self.quotient_id_space
    }

    /// The community of `node`, if it belongs to one.
    pub fn community(&self, node: DenseId) -> Option<DenseId> {
        match self.communities.get(usize::try_from(node).ok()?) {
            Some(&NO_COMMUNITY) | None => None,
            Some(&community) => Some(community),
        }
    }

    /// The stored communities, [`NO_COMMUNITY`] for none.
    pub(crate) fn stored(&self) -> &[u64] {
        &self.communities
    }

    /// The heap bytes it holds.
    pub fn bytes(&self) -> u64 {
        memory::held(&self.communities)
    }

    /// Checks it against the parent version's tombstones: one entry per
    /// parent node, and a community for exactly the live nodes.
    pub fn check_parent(&self, tombstones: &Tombstones) -> Result<(), GraphError> {
        if tombstones.id_space() != self.id_space() {
            return Err(GraphError::MembershipLength {
                expected: tombstones.id_space(),
                found: self.id_space(),
            });
        }
        let disagreeing = (0..self.id_space())
            .find(|&node| tombstones.contains(node) == self.community(node).is_some());
        match disagreeing {
            Some(node) => Err(GraphError::MembershipDisagrees { node }),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_node_has_its_community_or_none() {
        let membership = Membership::new(&[Some(1), None, Some(0)], 2).unwrap();
        assert_eq!(membership.id_space(), 3);
        assert_eq!(membership.quotient_id_space(), 2);
        assert_eq!(membership.community(0), Some(1));
        assert_eq!(membership.community(1), None);
        assert_eq!(membership.community(2), Some(0));
        assert_eq!(membership.community(3), None, "outside the parent's id space");
    }

    #[test]
    fn a_community_outside_the_quotient_is_refused() {
        assert_eq!(
            Membership::new(&[Some(0), Some(2)], 2),
            Err(GraphError::CommunityOutsideQuotient { node: 1, community: 2 })
        );
    }

    #[test]
    fn exactly_the_live_parent_nodes_have_a_community() {
        let mut tombstones = Tombstones::new(3).unwrap();
        tombstones.insert(1).unwrap();
        let membership = Membership::new(&[Some(0), None, Some(0)], 1).unwrap();
        assert_eq!(membership.check_parent(&tombstones), Ok(()));
        let live_without = Membership::new(&[Some(0), None, None], 1).unwrap();
        assert_eq!(
            live_without.check_parent(&tombstones),
            Err(GraphError::MembershipDisagrees { node: 2 })
        );
        let tombstoned_with = Membership::new(&[Some(0), Some(0), Some(0)], 1).unwrap();
        assert_eq!(
            tombstoned_with.check_parent(&tombstones),
            Err(GraphError::MembershipDisagrees { node: 1 })
        );
        let short = Membership::new(&[Some(0)], 1).unwrap();
        assert_eq!(
            short.check_parent(&tombstones),
            Err(GraphError::MembershipLength { expected: 3, found: 1 })
        );
    }
}
