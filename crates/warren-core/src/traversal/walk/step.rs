//! A step of a walk: which edges it follows, which nodes it may reach, and
//! how many hops it takes (DESIGN.md §9.2, `warren.step`).
//!
//! DESIGN.md calls a step whose hop range spans several lengths a
//! "variable-length segment"; the code says *hop range* instead, so that
//! "segment" keeps its one meaning, a stored part of a graph version.

use super::super::{NodeSet, TraversalError};
use crate::graph::Direction;
use crate::graph::{RelationIndex, RelationSelection};

/// How many hops a step takes: any number from `min` to `max` inclusive.
///
/// `min` may be 0: the step may then hand on the node it started from
/// without taking a hop, provided that node is in the step's filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HopRange {
    min: u32,
    max: u32,
}

impl HopRange {
    /// The largest `max` a hop range may have. A walk does work at every
    /// level up to `max` (on a cyclic graph the levels never empty), so an
    /// absurd range is refused rather than run until cancelled.
    pub const CAP: u32 = 1024;

    /// `min` to `max` hops; refused unless `min <= max <= CAP`.
    pub fn new(min: u32, max: u32) -> Result<Self, TraversalError> {
        if min > max || max > Self::CAP {
            return Err(TraversalError::InvalidHopRange { min, max });
        }
        Ok(Self { min, max })
    }

    /// Exactly `hops` hops.
    pub fn exactly(hops: u32) -> Result<Self, TraversalError> {
        Self::new(hops, hops)
    }

    /// The fewest hops.
    pub fn min(self) -> u32 {
        self.min
    }

    /// The most hops.
    pub fn max(self) -> u32 {
        self.max
    }
}

impl Default for HopRange {
    /// One hop, the `warren.s()` default.
    fn default() -> Self {
        Self { min: 1, max: 1 }
    }
}

/// One step of a walk.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// The relations the step follows; `None` follows every relation.
    pub relations: Option<Vec<RelationIndex>>,
    /// Which way the step follows edges.
    pub direction: Direction,
    /// The nodes the step may reach: every node a hop of the step enters,
    /// and the node it hands on, must be a member. `None` allows all.
    pub filter: Option<NodeSet>,
    /// How many hops the step takes.
    pub hops: HopRange,
}

impl Step {
    /// The relations the step follows, as a relation selection.
    pub fn selection(&self) -> RelationSelection<'_> {
        match &self.relations {
            None => RelationSelection::All,
            Some(only) => RelationSelection::Only(only),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traversal::TraversalError;

    #[test]
    fn a_hop_range_runs_from_min_to_max_inclusive() {
        let range = HopRange::new(1, 3).unwrap();
        assert_eq!((range.min(), range.max()), (1, 3));
        assert_eq!(HopRange::exactly(2), HopRange::new(2, 2));
        assert_eq!(HopRange::default(), HopRange::new(1, 1).unwrap());
    }

    #[test]
    fn zero_hops_is_allowed() {
        assert_eq!(HopRange::new(0, 0).unwrap().max(), 0);
    }

    #[test]
    fn max_below_min_is_an_error() {
        assert_eq!(HopRange::new(3, 2), Err(TraversalError::InvalidHopRange { min: 3, max: 2 }));
    }

    #[test]
    fn max_above_the_cap_is_an_error() {
        assert!(HopRange::new(0, HopRange::CAP).is_ok());
        assert_eq!(
            HopRange::new(1, HopRange::CAP + 1),
            Err(TraversalError::InvalidHopRange { min: 1, max: HopRange::CAP + 1 })
        );
    }
}
