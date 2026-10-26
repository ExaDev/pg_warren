"""The reference each engine's answer is checked against, made from the
dataset's `Source` under pg_warren's definitions (`corpus.oracle`) and
answered by the igraph reference, which the correctness suite checks
against NetworkX. An answer is in the form `check` takes for its
workload's kind:

- `reach`: a count; `depths`: node → hops; `paths`: "a->b" → hops, or cost
  when weighted (None when there is no path);
- `degree`: node → distinct out-neighbours; a self-loop's node may count
  it otherwise (igraph counts an undirected self-loop twice), a documented
  difference;
- `pagerank`: (convention, node → score), the convention one of
  `pagerank.CONVENTIONS`;
- `components`, `louvain`, `leiden`: node → label; components compared as
  partitions, communities held to the correctness suite's quality bound
  (at least the reference's Louvain modularity less `checks.QUALITY_SLACK`),
  each scored by the reference in the undirected view, so every engine's
  partition is measured alike;
- `triangles`: node → count, or {"total": count};
- `betweenness`: node → normalised betweenness (NetworkX's scale)."""

from corpus import checks

from . import pagerank

TOLERANCE = 1e-9  # PageRank, betweenness and costs: absolute
SELF_LOOP_DEGREE = "a self-loop's node counts it in its degree as the engine does (igraph twice, undirected)"


class Reference:
    def __init__(self, source, reference):
        self.source = source
        self.reference = reference
        self._louvain_modularity = None

    def check(self, workload, answer):
        kind = "community" if workload.kind in ("louvain", "leiden") else workload.kind
        return getattr(self, f"_{kind}")(workload, answer)

    def _reach(self, workload, count):
        depths = self.reference.bfs_depths(workload.source)
        expected = sum(1 for depth in depths.values() if depth <= workload.depth)
        return checks.compare(workload.name, {"count": count}, {"count": expected})

    def _depths(self, workload, depths):
        return checks.compare(workload.name, depths, self.reference.bfs_depths(workload.source))

    def _paths(self, workload, found):
        measure = self.reference.path_cost if workload.weighted else self.reference.path_length
        expected = {f"{a}->{b}": measure(a, b) for a, b in workload.pairs}
        return checks.compare(workload.name, found, expected, tolerance=TOLERANCE)

    def _degree(self, workload, degrees):
        return checks.compare(workload.name, degrees, self.reference.degrees("out"),
                              documented=(self.source.self_loop_nodes(), SELF_LOOP_DEGREE))

    def _pagerank(self, workload, answer):
        convention, scores = answer
        expected = pagerank.CONVENTIONS[convention](self.source, iterations=workload.iterations)
        return {**checks.compare(workload.name, scores, expected, tolerance=TOLERANCE), "convention": convention}

    def _components(self, workload, labels):
        return checks.compare_partitions(workload.name, labels, self.reference.components())

    def _triangles(self, workload, counts):
        expected = self.reference.triangles()
        if "total" in counts:
            return checks.compare(workload.name, counts, {"total": sum(expected.values()) // 3})
        return checks.compare(workload.name, counts, expected)

    def louvain_modularity(self):
        if self._louvain_modularity is None:
            self._louvain_modularity = self.reference.louvain()[1]
        return self._louvain_modularity

    def _community(self, workload, labels):
        modularity = self.reference.modularity(labels)
        return {**checks.quality_bound(workload.name, modularity, self.louvain_modularity()),
                "modularity": modularity, "communities": len(set(labels.values()))}

    def _betweenness(self, workload, values):
        return checks.compare(workload.name, values, self.reference.betweenness(), tolerance=TOLERANCE)
