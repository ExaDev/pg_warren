//! Appendix A.2 on a small instance: planted communities (a stochastic
//! block model), three kinds of ingest, and which of them the `edit` and
//! `subspace` distances tell apart.

use crate::compute::reference::{self, Edges};
use crate::compute::{ComputeMemory, Workers};
use crate::graph::{GraphCsr, RelationDictionary};
use crate::interrupts::Uninterrupted;
use crate::spectral::{
    edit_distance, eigen, subspace_distance, ComparedRows, EigenOptions, GraphMatrix, Operator,
    Precision, Resources, Which,
};

const COMMUNITIES: u64 = 4;
const MEMBERS: u64 = 30;
const BASE: u64 = COMMUNITIES * MEMBERS;
const INGEST: u64 = 48;
const P_IN: f64 = 0.3;
const P_OUT: f64 = 0.01;

/// A seeded xorshift generator of uniform values in `[0, 1)`.
struct Coins(u64);

impl Coins {
    fn flip(&mut self, p: f64) -> bool {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64) < p
    }
}

/// How the ingested nodes attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ingest {
    /// Each joins an existing community.
    Volume,
    /// They connect communities 0 and 1 densely, merging them.
    Bridge,
    /// They form a community of their own.
    Novel,
}

/// The community of node `node` after `ingest` (`None`: a bridge node).
fn community(node: u64, ingest: Option<Ingest>) -> Option<u64> {
    match (node < BASE, ingest) {
        (true, _) => Some(node / MEMBERS),
        (false, Some(Ingest::Volume)) => Some(node % COMMUNITIES),
        (false, Some(Ingest::Novel)) => Some(COMMUNITIES),
        (false, _) => None,
    }
}

/// The probability of an edge between `a` and `b`.
fn probability(a: u64, b: u64, ingest: Option<Ingest>) -> f64 {
    let bridged = |c: Option<u64>| c.is_none_or(|c| c < 2);
    match (community(a, ingest), community(b, ingest)) {
        (None, other) | (other, None) if bridged(other) => P_IN,
        (Some(x), Some(y)) if x == y => P_IN,
        _ => P_OUT,
    }
}

/// An undirected stochastic block model: the base graph, or the base graph
/// after `ingest`. The base edges are the same in every variant.
fn version(ingest: Option<Ingest>) -> (u64, Edges) {
    let nodes = if ingest.is_some() { BASE + INGEST } else { BASE };
    let (mut base, mut ingested) = (Coins(0x5eed), Coins(0xfeed));
    let mut edges = Edges::new();
    for b in 0..nodes {
        for a in 0..b {
            let added = match b < BASE {
                true => base.flip(probability(a, b, None)),
                false => ingested.flip(probability(a, b, ingest)),
            };
            if added {
                edges.extend([(0, a, b, 1.0), (0, b, a, 1.0)]);
            }
        }
    }
    (nodes, edges)
}

fn graph(ingest: Option<Ingest>) -> GraphCsr {
    let (nodes, edges) = version(ingest);
    reference::graph(nodes, 1, &edges, false, true)
}

/// The subspace distance between the `COMMUNITIES` smallest normalised
/// Laplacian eigenvectors of the base graph and of `after`.
fn subspace(base: &GraphCsr, after: &GraphCsr) -> f64 {
    let workers = Workers::new(2).unwrap();
    let memory = ComputeMemory::new(1 << 26);
    let mut resources =
        Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted };
    let options = EigenOptions {
        which: Which::Smallest,
        precision: Precision::F64,
        ..EigenOptions::new(COMMUNITIES as usize)
    };
    let symmetric =
        GraphMatrix { symmetrize: false, ..GraphMatrix::new(Operator::NormalizedLaplacian) };
    let before = eigen(&mut resources, base, &symmetric, &options).unwrap();
    let after = eigen(&mut resources, after, &symmetric, &options).unwrap();
    assert!(before.converged && after.converged);
    let before: Vec<_> = before.pairs.iter().map(|p| &p.vector).collect();
    let after: Vec<_> = after.pairs.iter().map(|p| &p.vector).collect();
    subspace_distance(&mut resources, &before, &after, ComparedRows::Shared).unwrap().projection
}

/// Over the nodes both versions share, added volume barely turns the
/// community subspace, while a new community or a merger of two turns about
/// one of its dimensions away (`√2` would be a whole one). Which of those
/// two turns further depends on the sizes involved, so this small
/// instance does not order them (Appendix A.2 does, at its scale).
#[test]
fn the_subspace_distance_tells_volume_from_restructuring() {
    let base = graph(None);
    let [volume, novel, bridge] =
        [Ingest::Volume, Ingest::Novel, Ingest::Bridge].map(|i| graph(Some(i)));
    let d = |after: &GraphCsr| subspace(&base, after);
    let (dv, dn, db) = (d(&volume), d(&novel), d(&bridge));
    assert!(dv < 0.5, "volume {dv}");
    assert!(dn > 1.0 && db > 1.0, "novel {dn}, bridge {db}");
    assert_eq!(d(&bridge), db, "the same comparison twice gives the same result");
    assert_eq!(d(&base), 0.0);
}

/// Ingest only adds edges, so the `edit` distance is a function of how
/// many: it measures volume and cannot tell the three apart.
#[test]
fn the_edit_distance_measures_only_volume() {
    let base = graph(None);
    let base_edges = base.all_relations().forward().edge_count();
    for ingest in [Ingest::Volume, Ingest::Novel, Ingest::Bridge] {
        let after = graph(Some(ingest));
        let mut names = RelationDictionary::new();
        names.intern("r0").unwrap();
        let d = edit_distance((&names, &base), (&names, &after)).overall;
        assert_eq!((d.removed, d.union), (0, after.all_relations().forward().edge_count()));
        assert_eq!(d.distance, d.added as f64 / (base_edges + d.added) as f64);
    }
}
