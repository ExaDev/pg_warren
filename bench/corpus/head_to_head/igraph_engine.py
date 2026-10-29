"""python-igraph (igraph's C core) in this process: the dataset's tables read
with the csv module (`ingest`), then the graph built as pg_warren builds
it (`build`): parallel edges folded into one, their weights summed, and
the undirected view (each pair once, A + Aᵀ's weights) that triangles and
the community detections read, as GDS's second projection does.

igraph's algorithms are single-threaded, and an algorithm holds nothing
between calls, so a cold run is the first call and a warm one the same
call again. `run` keeps the result in Python; `answer` names it by the
dataset's ids."""

import csv
import random
import time
import warnings

import igraph as ig

LOUVAIN_SEED = 1


def _rows(path):
    with open(path, newline="", encoding="utf-8") as f:
        yield from csv.reader(f)


class IGraphEngine:
    name = "igraph"

    def __init__(self, dataset, directory):
        self.dataset, self.directory = dataset, directory
        self.graph = self.view = self.simple = self.result = None

    def describe(self):
        return {"version": ig.__version__, "threads": 1,
                "settings": {"pagerank": "PRPACK (solved)", "louvain": "community_multilevel",
                             "leiden": "community_leiden, modularity, 2 iterations", "seed": LOUVAIN_SEED,
                             "paths": "get_shortest_path per pair (BFS or Dijkstra, stopping at the target)"}}

    def load(self):
        started = time.perf_counter()
        self.names = [row[0] for row in _rows(self.directory / "nodes.csv")]
        self.index = {name: i for i, name in enumerate(self.names)}
        weighted = self.dataset.weighted
        pairs, weights = [], []
        for row in _rows(self.directory / "edges.csv"):
            pairs.append((self.index[row[0]], self.index[row[1]]))
            weights.append(float(row[-1]) if weighted else 1.0)
        ingest = time.perf_counter() - started
        started = time.perf_counter()
        self._build(pairs, weights)
        return {"ingest": [ingest], "build": [time.perf_counter() - started]}

    def _build(self, pairs, weights):
        graph = ig.Graph(n=len(self.names), edges=pairs, directed=self.dataset.directed,
                         edge_attrs={"weight": weights})
        # pg_warren's relations are not edge attributes here: an unweighted
        # pair in several relations is one edge of weight 1, as spmv's A.
        graph.simplify(multiple=True, loops=False, combine_edges="sum" if self.dataset.weighted else "first")
        self.graph = graph
        self.view = graph.as_undirected(mode="collapse", combine_edges="sum") if graph.is_directed() else graph
        self.simple = self.view.copy().simplify(multiple=True, loops=True)

    def unsupported(self, workload):
        return None

    def make_cold(self, workload):
        return {}

    def reset(self, workload):
        self.result = None

    def _weights(self):
        return "weight" if self.dataset.weighted else None

    def run(self, workload):
        self.result = getattr(self, f"_{workload.kind}")(workload)

    def _degree(self, workload):
        # Simplified, so distinct neighbours; an undirected self-loop counts twice.
        return self.graph.degree(mode="out")

    def _reach(self, workload):
        return self.graph.neighborhood_size(self.index[workload.source], order=workload.depth, mode="out")

    def _depths(self, workload):
        return self.graph.distances(source=[self.index[workload.source]], mode="out")[0]

    def _paths(self, workload):
        # get_shortest_path stops once it reaches the target and reads
        # neighbours lazily; distances with a target also stops there, but
        # first builds the whole graph's adjacency list on every call.
        weights = self.graph.es["weight"] if workload.weighted else None
        found = []
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", RuntimeWarning)  # "Couldn't reach some vertices"
            for a, b in workload.pairs:
                edges = self.graph.get_shortest_path(self.index[a], to=self.index[b], weights=weights, mode="out",
                                                     output="epath")
                if not edges and a != b:
                    found.append(None)
                else:
                    found.append(sum(weights[e] for e in edges) if weights else len(edges))
        return found

    def _pagerank(self, workload):
        return self.graph.pagerank(directed=True, weights=self._weights(), implementation="prpack")

    def _components(self, workload):
        return self.graph.connected_components(mode="weak").membership

    def _triangles(self, workload):
        coefficients = self.simple.transitivity_local_undirected(mode="zero")
        return [round(c * d * (d - 1) / 2) for c, d in zip(coefficients, self.simple.degree())]

    def _seeded(self, detect):
        ig.set_random_number_generator(random.Random(LOUVAIN_SEED))
        try:
            return detect().membership
        finally:
            ig.set_random_number_generator(random)

    def _louvain(self, workload):
        return self._seeded(lambda: self.view.community_multilevel(weights="weight"))

    def _leiden(self, workload):
        return self._seeded(lambda: self.view.community_leiden(objective_function="modularity", weights="weight",
                                                               n_iterations=2))

    def _betweenness(self, workload):
        raw = self.graph.betweenness(directed=True, weights=self._weights())
        n = len(self.names)
        scale = (1 if self.graph.is_directed() else 2) / ((n - 1) * (n - 2)) if n > 2 else 1
        return [value * scale for value in raw]

    def answer(self, workload):
        kind, result = workload.kind, self.result
        if kind == "reach":
            return result
        if kind == "paths":
            return {f"{a}->{b}": found for (a, b), found in zip(workload.pairs, result)}
        if kind == "depths":
            return {name: int(d) for name, d in zip(self.names, result) if d != float("inf")}
        named = dict(zip(self.names, result))
        return ("fixed point", named) if kind == "pagerank" else named

    def close(self):
        self.graph = self.view = self.simple = self.result = None
