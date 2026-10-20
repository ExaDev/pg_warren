//! Reads a fixture's `edges.csv` (`src,dst`, ids `1..=nodes`, a header
//! line), or a random graph, as a directed, unweighted graph with its
//! reverse CSR, for the traversal examples.

use std::error::Error;
use std::fs::File;
use std::io::{BufRead, BufReader};

use warren_core::graph::{EdgeList, GraphCsr, RelationDictionary};

/// The directed graph of the edges in `path`, over `nodes` dense ids
/// (external id `i` is dense id `i - 1`), with a reverse CSR.
pub fn directed_graph(path: &str, nodes: u64) -> Result<GraphCsr, Box<dyn Error>> {
    let relation = RelationDictionary::new().intern("edge")?;
    let mut edges = EdgeList::for_id_space(nodes, false);
    for line in BufReader::new(File::open(path)?).lines().skip(1) {
        let line = line?;
        let (src, dst) = line.split_once(',').ok_or("expected src,dst")?;
        edges.push(relation, src.parse::<u64>()? - 1, dst.parse::<u64>()? - 1, 1.0)?;
    }
    Ok(GraphCsr::build(edges, 1, nodes, true)?)
}

/// A G(n, m) graph with replacement: `edges` uniformly random directed
/// edges over `nodes` dense ids (a seeded 64-bit LCG), with a reverse CSR.
pub fn random_graph(nodes: u64, edges: u64) -> Result<GraphCsr, Box<dyn Error>> {
    let relation = RelationDictionary::new().intern("edge")?;
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state =
            state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (state >> 16) % nodes
    };
    let mut list = EdgeList::for_id_space(nodes, false);
    for _ in 0..edges {
        let (source, target) = (next(), next());
        list.push(relation, source, target, 1.0)?;
    }
    Ok(GraphCsr::build(list, 1, nodes, true)?)
}
