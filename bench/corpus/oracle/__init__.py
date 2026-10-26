"""The oracle port: references for a dataset, read from its tables (the
CSVs `tables` wrote) under pg_warren's definitions, so a difference is a
finding and not a convention, whichever library answers. `Source` reads
the tables once into those definitions; each library's reference is made
from it and answers the same questions (`NetworkXReference` has them
documented), so `checks` never asks which library it holds.

The definitions:
- parallel edges are one edge, their weights summed (§14 item 4); an edge
  in several relations is one edge of the all-relations graph, weighing
  its relations' sum (a strength, as `spmv` reads it) or, as a length, its
  cheapest relation's weight (`dijkstra`, §5.3);
- degree counts each neighbour once, a self-loop's node included (§9.3);
- triangles, clustering and core numbers are of the undirected simple
  graph (§6.7);
- betweenness is along edges, normalised as NetworkX normalises it, its
  paths costing their cheapest relations when weighted (§6.7);
- modularity is of the undirected view B (A + Aᵀ for a directed graph, A
  for an undirected one), in which a self-loop counts once in its node's
  degree (`Source.undirected_view`). A is `spmv`'s: an unweighted pair in
  k relations weighs 1 (docs/compute.md), or k with `per_relation=True`.
Ids stay the strings the tables hold."""

from .igraph_reference import IGraphReference
from .networkx_reference import NetworkXReference
from .source import Edge, Source

LIBRARIES = {reference.NAME: reference for reference in (NetworkXReference, IGraphReference)}

__all__ = ["Edge", "IGraphReference", "LIBRARIES", "NetworkXReference", "Source"]
