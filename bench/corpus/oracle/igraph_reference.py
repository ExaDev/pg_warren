"""The igraph reference: each answer of the oracle port, under the
definitions `oracle` states, from igraph's own algorithms (python-igraph,
igraph's C core). Independent of NetworkX and much faster, so it is a
second opinion on every check and the one that can afford exact
betweenness on graphs of thousands of nodes.

Where igraph's convention differs from the port's, the graph it is given
says what is meant: PageRank runs on the directed graph that follows an
undirected edge both ways and a self-loop once (as pg_warren stores it and
NetworkX's `to_directed` makes it), and betweenness, which igraph counts
over unordered pairs on an undirected graph, is rescaled to NetworkX's."""

import math
import random

import igraph as ig

LOUVAIN_SEED = 1


class IGraphReference:
    NAME = "igraph"
    VERSION = ig.__version__
    PAGERANK_SOLVED = True  # PRPACK solves for the fixed point
    # The largest N·M (nodes times stored edges) exact betweenness is checked
    # on: in C, 1.5 s for facebook (3.6·10⁸) and 46 s for WN18RR (3.8·10⁹),
    # the tiny tier's largest.
    BETWEENNESS_WORK = 5 * 10**9

    def __init__(self, source):
        self.source = source
        self.weighted = source.weighted
        self.names = list(source.nodes)
        self.index = {name: i for i, name in enumerate(self.names)}
        pairs = [(self.index[a], self.index[b]) for a, b in source.edges]
        self.graph = ig.Graph(n=len(self.names), edges=pairs, directed=source.directed,
                              edge_attrs={"weight": [e.weight for e in source.edges.values()],
                                          "cost": [e.cost for e in source.edges.values()]})

    def _named(self, values):
        return dict(zip(self.names, values))

    def degrees(self, direction):
        """Distinct neighbours: `out`, `in` or `both` (the same undirected)."""
        mode = {"out": "out", "in": "in", "both": "all"}[direction] if self.graph.is_directed() else "all"
        return self._named(len(set(row)) for row in self.graph.get_adjlist(mode=mode))

    def bfs_depths(self, source):
        """Hops along edges to every node `source` reaches."""
        depths = self.graph.distances(source=[self.index[source]], mode="out")[0]
        return {name: int(d) for name, d in zip(self.names, depths) if not math.isinf(d)}

    def components(self):
        """Weak components: node → a label of its component."""
        return self._named(self.graph.connected_components(mode="weak").membership)

    def _distance(self, source, target, weights):
        found = self.graph.distances(source=[self.index[source]], target=[self.index[target]], weights=weights,
                                     mode="out")[0][0]
        return None if math.isinf(found) else found

    def path_length(self, source, target):
        """Hops of a shortest path along edges, None if there is none."""
        return self._distance(source, target, None)

    def path_cost(self, source, target):
        """The cost of a cheapest path along edges, None if there is none."""
        found = self._distance(source, target, "cost")
        return None if found is None else float(found)

    def has_edge(self, source, target):
        return self.graph.get_eid(self.index[source], self.index[target], directed=True, error=False) >= 0

    def _stored(self):
        """The directed graph pg_warren stores: an undirected edge both
        ways, a self-loop once."""
        if self.graph.is_directed():
            return self.graph
        edges, weights = self.graph.get_edgelist(), self.graph.es["weight"]
        reverse = [((b, a), w) for (a, b), w in zip(edges, weights) if a != b]
        return ig.Graph(n=len(self.names), edges=edges + [e for e, _ in reverse], directed=True,
                        edge_attrs={"weight": weights + [w for _, w in reverse]})

    def pagerank(self, damping=0.85):
        return self._named(self._stored().pagerank(damping=damping, directed=True,
                                                   weights="weight" if self.weighted else None,
                                                   implementation="prpack"))

    def _simple(self):
        pairs = {(min(a, b), max(a, b)) for a, b in self.graph.get_edgelist() if a != b}
        return ig.Graph(n=len(self.names), edges=sorted(pairs), directed=False)

    def triangles(self):
        counts = [0] * len(self.names)
        for triangle in self._simple().list_triangles():
            for node in triangle:
                counts[node] += 1
        return self._named(counts)

    def clustering(self):
        return self._named(self._simple().transitivity_local_undirected(mode="zero"))

    def core_number(self):
        return self._named(self._simple().coreness())

    def betweenness(self):
        """Exact, normalised as NetworkX, along edges; weighted by cost if
        the graph is."""
        raw = self.graph.betweenness(directed=True, weights="cost" if self.weighted else None)
        n = len(self.names)
        if n <= 2:
            return self._named(raw)
        # NetworkX sums over ordered pairs; igraph's undirected sum is over
        # unordered ones, half of that.
        scale = (1 if self.graph.is_directed() else 2) / ((n - 1) * (n - 2))
        return self._named(value * scale for value in raw)

    def _view(self, per_relation):
        view = self.source.undirected_view(per_relation)
        return ig.Graph(n=len(self.names), edges=[(self.index[a], self.index[b]) for a, b in view],
                        directed=False, edge_attrs={"weight": list(view.values())})

    def modularity(self, labels, per_relation=False):
        """The modularity of the partition `labels` (node → label, every
        node labelled) in the undirected view."""
        return self._modularity(labels, self._view(per_relation))

    def _modularity(self, labels, view):
        numbered = {}
        membership = [numbered.setdefault(labels[name], len(numbered)) for name in self.names]
        return view.modularity(membership, weights="weight", directed=False)

    def louvain(self):
        """igraph's multilevel (Louvain) partition of the undirected view
        (node → label), seeded, and its modularity."""
        view = self._view(False)
        ig.set_random_number_generator(random.Random(LOUVAIN_SEED))
        try:
            membership = view.community_multilevel(weights="weight").membership
        finally:
            ig.set_random_number_generator(random)
        labels = self._named(membership)
        return labels, self._modularity(labels, view)
