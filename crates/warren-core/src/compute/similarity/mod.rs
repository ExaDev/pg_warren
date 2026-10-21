//! Neighbourhood similarity for link prediction (DESIGN.md §6.7, planned
//! library group 2): Jaccard, Adamic–Adar and common neighbours over the
//! undirected simple graph under the selected relations (directions and
//! self-loops ignored), as NetworkX's `jaccard_coefficient`,
//! `adamic_adar_index` and `common_neighbors` score a pair.
//!
//! For nodes `u`, `v` with neighbourhoods `N(u)`, `N(v)`: common neighbours
//! `|N(u) ∩ N(v)|`; Jaccard `|N(u) ∩ N(v)| / |N(u) ∪ N(v)|` (0 when both
//! are empty); Adamic–Adar `Σ_{w ∈ N(u) ∩ N(v)} 1 / ln |N(w)|`, summed in
//! ascending dense id. A common neighbour of two distinct nodes has degree
//! at least 2; only a node paired with itself can share a neighbour of
//! degree 1, whose term is `1 / ln 1 = +∞` (NetworkX raises
//! `ZeroDivisionError`).
//!
//! Three forms: [`similarity_of_pairs`] scores given pairs (NetworkX's
//! `ebunch`) by intersecting sorted rows as `triangles` does;
//! [`most_similar`] ranks one node's candidates and [`similar_pairs`] each
//! node's, by expanding two hops (`Σ_{w ∈ N(u)} |N(w)|` per node). A
//! candidate is a node sharing a neighbour with `u` that is neither `u` nor
//! a neighbour of `u` (NetworkX's default `ebunch`, the non-edges, less the
//! pairs that score 0); candidates rank by score, then by dense id.

mod neighbourhoods;
mod pairs;
mod ranking;
#[cfg(test)]
#[cfg(not(miri))]
mod tests;

use std::str::FromStr;

pub use pairs::{similarity_of_pairs, Scores};
pub use ranking::{most_similar, similar_pairs, Similar, SimilarPairs};

use super::ComputeError;
use crate::graph::{GraphCsr, RelationSelection, Tombstones};

/// A neighbourhood similarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Similarity {
    /// Shared neighbours over all neighbours.
    Jaccard,
    /// Shared neighbours, each weighed by one over the log of its degree.
    AdamicAdar,
    /// The number of shared neighbours.
    CommonNeighbours,
}

impl FromStr for Similarity {
    type Err = ComputeError;

    /// `jaccard`, `adamic_adar` or `common_neighbours`.
    fn from_str(name: &str) -> Result<Self, ComputeError> {
        match name {
            "jaccard" => Ok(Self::Jaccard),
            "adamic_adar" => Ok(Self::AdamicAdar),
            "common_neighbours" => Ok(Self::CommonNeighbours),
            _ => Err(ComputeError::UnknownSimilarity(name.to_owned())),
        }
    }
}

/// The undirected simple graph a similarity reads.
#[derive(Debug, Clone, Copy)]
pub struct Neighbourhood<'a, 'g> {
    /// The graph version.
    pub graph: &'g GraphCsr,
    /// The relations whose edges join neighbours.
    pub relations: RelationSelection<'a>,
    /// The version's tombstones: tombstoned nodes have no neighbours.
    pub tombstones: &'a Tombstones,
}

/// What two nodes share: their common neighbours, and the sum of one over
/// the log of each one's degree.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Shared {
    count: u64,
    adamic_adar: f64,
}

impl Similarity {
    /// The score of a pair of degrees `degrees` sharing `shared`.
    fn score(self, shared: Shared, degrees: (u64, u64)) -> f64 {
        match self {
            Self::CommonNeighbours => shared.count as f64,
            Self::AdamicAdar => shared.adamic_adar,
            Self::Jaccard => match degrees.0 + degrees.1 - shared.count {
                0 => 0.0,
                union => shared.count as f64 / union as f64,
            },
        }
    }

    /// Whether scores need the shared neighbours' degrees.
    fn weighs_neighbours(self) -> bool {
        self == Self::AdamicAdar
    }
}

/// One over the log of a shared neighbour's degree.
fn adamic_adar_term(degree: u64) -> f64 {
    1.0 / (degree as f64).ln()
}
