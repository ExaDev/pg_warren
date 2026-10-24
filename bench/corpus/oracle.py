"""NetworkX references for a dataset, read from its tables (the CSVs
`tables` wrote) under pg_warren's definitions, so a difference is a
finding and not a convention:
- parallel edges are one edge, their weights summed (§14 item 4); an edge
  in several relations is one edge of the all-relations graph;
- degree counts each neighbour once, a self-loop's node included (§9.3);
- triangles, clustering and core numbers are of the undirected simple
  graph (§6.7);
- modularity is of the undirected view B (A + Aᵀ for a directed graph, A
  for an undirected one), in which a self-loop counts once in its node's
  degree; NetworkX counts a self-loop's weight twice, so the view gives it
  half of B's diagonal (§6.7, docs/communities.md). A is `spmv`'s: an
  unweighted pair in k relations weighs 1 (docs/compute.md), or k with
  `per_relation=True`.
Ids stay the strings the tables hold."""

import csv
from dataclasses import dataclass

import networkx as nx

PAGERANK_TOLERANCE = 1e-12
LOUVAIN_SEED = 1


def _rows(path):
    with open(path, newline="", encoding="utf-8") as f:
        yield from csv.reader(f)


@dataclass
class Reference:
    graph: nx.Graph  # a DiGraph when the dataset is directed
    weighted: bool

    @classmethod
    def read(cls, directory, dataset):
        graph = nx.DiGraph() if dataset.directed else nx.Graph()
        graph.add_nodes_from(row[0] for row in _rows(directory / "nodes.csv"))
        weights = {}
        for row in _rows(directory / "edges.csv"):
            source, target = row[0], row[1]
            key = (source, target) if dataset.directed or source <= target else (target, source)
            weight = float(row[-1]) if dataset.weighted else 1.0
            relation = row[2] if dataset.relations else None
            weights.setdefault(key, {})
            # Duplicates within a relation sum (the build's deduplication);
            # the all-relations graph sums across relations too (A of §6.3).
            weights[key][relation] = weights[key].get(relation, 0.0) + weight
        for (source, target), per_relation in weights.items():
            graph.add_edge(source, target, weight=sum(per_relation.values()), relations=len(per_relation))
        return cls(graph, dataset.weighted)

    def _weight(self):
        return "weight" if self.weighted else None

    def degrees(self, direction):
        g = self.graph
        if not g.is_directed():
            return {n: len(set(g.neighbors(n))) for n in g}
        if direction == "out":
            return {n: len(set(g.successors(n))) for n in g}
        if direction == "in":
            return {n: len(set(g.predecessors(n))) for n in g}
        return {n: len(set(g.successors(n)) | set(g.predecessors(n))) for n in g}

    def bfs_depths(self, source):
        return dict(nx.single_source_shortest_path_length(self.graph, source))

    def components(self):
        parts = nx.weakly_connected_components(self.graph) if self.graph.is_directed() \
            else nx.connected_components(self.graph)
        return {node: i for i, part in enumerate(parts) for node in part}

    def path_length(self, source, target):
        """Hops of a shortest path along edges, None if there is none."""
        try:
            return nx.shortest_path_length(self.graph, source, target)
        except nx.NetworkXNoPath:
            return None

    def path_cost(self, source, target):
        try:
            return nx.dijkstra_path_length(self.graph, source, target, weight="weight")
        except nx.NetworkXNoPath:
            return None

    def has_edge(self, source, target):
        return self.graph.has_edge(source, target)

    def pagerank(self, damping=0.85):
        return nx.pagerank(self.graph, alpha=damping, weight=self._weight(), tol=PAGERANK_TOLERANCE, max_iter=10_000)

    def _simple(self):
        simple = nx.Graph(self.graph.to_undirected(as_view=True)) if self.graph.is_directed() else nx.Graph(self.graph)
        simple.remove_edges_from(list(nx.selfloop_edges(simple)))
        return simple

    def triangles(self):
        return nx.triangles(self._simple())

    def clustering(self):
        return nx.clustering(self._simple())

    def core_number(self):
        return nx.core_number(self._simple())

    def self_loop_nodes(self):
        return {node for node, _ in nx.selfloop_edges(self.graph)}

    def undirected_view(self, per_relation=False):
        view = nx.Graph()
        view.add_nodes_from(self.graph)
        for source, target, data in self.graph.edges(data=True):
            weight = data["weight"] if self.weighted else float(data["relations"] if per_relation else 1)
            if source == target:
                # B_uu is 2·A_uu directed and A_uu undirected; NetworkX
                # counts a self-loop's weight twice, so it gets half of B_uu.
                weight = weight if self.graph.is_directed() else weight / 2
            if view.has_edge(source, target):
                view[source][target]["weight"] += weight
            else:
                view.add_edge(source, target, weight=weight)
        return view

    def modularity(self, labels, view=None):
        view = self.undirected_view() if view is None else view
        communities = {}
        for node, label in labels.items():
            communities.setdefault(label, set()).add(node)
        return nx.community.modularity(view, communities.values(), weight="weight")

    def louvain(self):
        """NetworkX's Louvain partition of the undirected view (node → label)
        and its modularity."""
        view = self.undirected_view()
        parts = nx.community.louvain_communities(view, weight="weight", seed=LOUVAIN_SEED)
        labels = {node: i for i, part in enumerate(parts) for node in part}
        return labels, self.modularity(labels, view)
