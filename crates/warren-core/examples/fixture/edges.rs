//! Reads a fixture's `edges.csv` (`src,dst`, ids `1..=nodes`, a header
//! line) as an undirected, unweighted graph, for the spectral examples.

use std::error::Error;
use std::fs::File;
use std::io::{BufRead, BufReader};

use warren_core::graph::{EdgeList, GraphCsr, RelationDictionary};

/// The undirected graph of the edges in `path`, over `nodes` dense ids.
pub fn undirected_graph(path: &str, nodes: u64) -> Result<GraphCsr, Box<dyn Error>> {
    let relation = RelationDictionary::new().intern("edge")?;
    let mut edges = EdgeList::for_id_space(nodes, false);
    for line in BufReader::new(File::open(path)?).lines().skip(1) {
        let line = line?;
        let (src, dst) = line.split_once(',').ok_or("expected src,dst")?;
        let (src, dst) = (src.parse::<u64>()? - 1, dst.parse::<u64>()? - 1);
        edges.push(relation, src, dst, 1.0)?;
        if src != dst {
            edges.push(relation, dst, src, 1.0)?;
        }
    }
    Ok(GraphCsr::build(edges, 1, nodes, false)?)
}
