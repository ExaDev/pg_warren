"""What each dataset runs, the same for every engine: a core set common to
igraph and Neo4j's Graph Data Science library, from one start node and one
set of seeded pairs (the correctness suite's, `checks.path_pairs`).

- `degree`: every node's distinct out-neighbours (both ways undirected);
- `bfs depth 6`: how many nodes the start reaches within 6 hops;
- `bfs depths`: the hops from the start to every node it reaches (the
  single-source shortest path, unweighted);
- `shortest paths`: hops, or the cheapest cost when the graph is weighted,
  between each seeded pair;
- `pagerank 20 iterations`: 20 propagation steps, damping 0.85;
- `components`: weak components;
- `triangles`, `louvain`, `leiden`: of the undirected view;
- `betweenness`: exact, normalised, where N·M is at most `betweenness_work`."""

from dataclasses import dataclass

from corpus import checks

DEPTH = 6
PAGERANK_ITERATIONS = 20
PATH_PAIRS = checks.PATH_PAIRS
BETWEENNESS_WORK = 5 * 10**8  # facebook's 3.6·10⁸ in; WN18RR's 3.8·10⁹ out


@dataclass(frozen=True)
class Workload:
    name: str
    kind: str
    source: str | None = None
    depth: int | None = None
    pairs: tuple = ()
    weighted: bool = False
    iterations: int | None = None


def start(source):
    """The traversals' start: the node with most out-edges of a directed
    graph (its busiest node may only receive), the hub of an undirected
    one; ties to the smallest id."""
    if not source.directed:
        return source.hub()
    out = dict.fromkeys(source.nodes, 0)
    for node, _ in source.edges:
        out[node] += 1
    return min(out, key=lambda node: (-out[node], node))


def plan(source, betweenness_work=BETWEENNESS_WORK):
    begin = start(source)
    pairs = tuple(checks.path_pairs(sorted(source.nodes), PATH_PAIRS, checks.PATH_SEED))
    planned = [
        Workload("degree", "degree"),
        Workload(f"bfs depth {DEPTH}", "reach", source=begin, depth=DEPTH),
        Workload("bfs depths", "depths", source=begin),
        Workload("shortest paths", "paths", pairs=pairs, weighted=source.weighted),
        Workload(f"pagerank {PAGERANK_ITERATIONS} iterations", "pagerank", iterations=PAGERANK_ITERATIONS),
        Workload("components", "components"),
        Workload("triangles", "triangles"),
        Workload("louvain", "louvain"),
        Workload("leiden", "leiden"),
    ]
    if len(source.nodes) * len(source.edges) <= betweenness_work:
        planned.append(Workload("betweenness", "betweenness"))
    return planned
