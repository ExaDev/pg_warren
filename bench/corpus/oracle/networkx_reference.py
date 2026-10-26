"""The NetworkX reference: each answer of the oracle port, under the
definitions `oracle` states, from NetworkX's own algorithms."""

import networkx as nx

PAGERANK_STOP = 1e-12  # tol: stop when the L1 change is below N·tol, as pg_warren
LOUVAIN_SEED = 1


class NetworkXReference:
    NAME = "networkx"
    VERSION = nx.__version__
    PAGERANK_SOLVED = False  # its iteration is pg_warren's (§6.7)
    # The largest N·M (nodes times stored edges) exact betweenness is checked
    # on: in Python, 3 s for polblogs (2.8·10⁷) and CoDEx-S (6.7·10⁷), 29 s
    # for transport-airline (1.3·10⁸) and 74 s for facebook (3.6·10⁸).
    BETWEENNESS_WORK = 10**8

    def __init__(self, source):
        self.weighted = source.weighted
        self.source = source
        self.graph = nx.DiGraph() if source.directed else nx.Graph()
        self.graph.add_nodes_from(source.nodes)
        for (a, b), edge in source.edges.items():
            self.graph.add_edge(a, b, weight=edge.weight, cost=edge.cost)

    def _weight(self):
        return "weight" if self.weighted else None

    def degrees(self, direction):
        """Distinct neighbours: `out`, `in` or `both` (the same undirected)."""
        g = self.graph
        if not g.is_directed():
            return {n: len(set(g.neighbors(n))) for n in g}
        if direction == "out":
            return {n: len(set(g.successors(n))) for n in g}
        if direction == "in":
            return {n: len(set(g.predecessors(n))) for n in g}
        return {n: len(set(g.successors(n)) | set(g.predecessors(n))) for n in g}

    def bfs_depths(self, source):
        """Hops along edges to every node `source` reaches."""
        return dict(nx.single_source_shortest_path_length(self.graph, source))

    def components(self):
        """Weak components: node → a label of its component."""
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
        """The cost of a cheapest path along edges, None if there is none."""
        try:
            return nx.dijkstra_path_length(self.graph, source, target, weight="cost")
        except nx.NetworkXNoPath:
            return None

    def has_edge(self, source, target):
        return self.graph.has_edge(source, target)

    def pagerank(self, damping=0.85):
        return nx.pagerank(self.graph, alpha=damping, weight=self._weight(), tol=PAGERANK_STOP, max_iter=10_000)

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

    def betweenness(self):
        """Exact, normalised, along edges; weighted by cost if the graph is."""
        return nx.betweenness_centrality(self.graph, normalized=True, weight="cost" if self.weighted else None)

    def _view(self, per_relation):
        view = nx.Graph()
        view.add_nodes_from(self.graph)
        view.add_weighted_edges_from((a, b, w) for (a, b), w in self.source.undirected_view(per_relation).items())
        return view

    def modularity(self, labels, per_relation=False):
        """The modularity of the partition `labels` (node → label) in the
        undirected view."""
        return self._modularity(labels, self._view(per_relation))

    @staticmethod
    def _modularity(labels, view):
        communities = {}
        for node, label in labels.items():
            communities.setdefault(label, set()).add(node)
        return nx.community.modularity(view, communities.values(), weight="weight")

    def louvain(self):
        """NetworkX's Louvain partition of the undirected view (node → label)
        and its modularity."""
        view = self._view(False)
        parts = nx.community.louvain_communities(view, weight="weight", seed=LOUVAIN_SEED)
        labels = {node: i for i, part in enumerate(parts) for node in part}
        return labels, self._modularity(labels, view)
