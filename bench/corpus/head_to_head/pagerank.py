"""PageRank as each engine computes it, from a dataset's `Source`, so each
engine's answer is checked against what it claims to compute:

- `normalised`: pg_warren's library and NetworkX's iteration: x₀ = 1/N,
  then x = d·(Aᵀ(x / out) + dangling/N) + (1 − d)/N, a dangling node's rank
  spread evenly; the scores sum to 1;
- `fixed_point`: the same, solved (igraph's PRPACK);
- `unnormalised`: Neo4j GDS: x₀ = 1 − d, then x = (1 − d) + d·Aᵀ(x / out),
  a dangling node's rank lost; the scores sum to about N less what
  dangling nodes lose. GDS counts x₀ as its first iteration, so
  `maxIterations: 21` is 20 propagation steps.

A is the graph pg_warren stores: each deduplicated edge once (an
undirected one both ways, a self-loop once), weighing its weight when the
graph is weighted and 1 otherwise."""

import numpy as np
from scipy import sparse

DAMPING = 0.85
FIXED_POINT_TOLERANCE = 1e-15  # L1 change at which `fixed_point` stops


def _matrix(source):
    index = {node: i for i, node in enumerate(source.nodes)}
    rows, columns, weights = [], [], []
    for (a, b), edge in source.edges.items():
        weight = edge.weight if source.weighted else 1.0
        rows.append(index[a]), columns.append(index[b]), weights.append(weight)
        if not source.directed and a != b:
            rows.append(index[b]), columns.append(index[a]), weights.append(weight)
    n = len(source.nodes)
    a = sparse.csr_matrix((weights, (rows, columns)), shape=(n, n))
    return a.T.tocsr(), np.asarray(a.sum(axis=1)).ravel()


def _named(source, x):
    return dict(zip(source.nodes, x.tolist()))


def _spread(transposed, out, x):
    return transposed @ np.divide(x, out, out=np.zeros_like(x), where=out > 0)


def _normalised_steps(source, damping):
    transposed, out = _matrix(source)
    n = len(source.nodes)
    x = np.full(n, 1.0 / n)
    while True:
        dangling = x[out == 0].sum()
        x = damping * (_spread(transposed, out, x) + dangling / n) + (1 - damping) / n
        yield x


def normalised(source, iterations, damping=DAMPING):
    x = np.full(len(source.nodes), 1.0 / len(source.nodes))
    steps = _normalised_steps(source, damping)
    for _ in range(iterations):
        x = next(steps)
    return _named(source, x)


def fixed_point(source, iterations=None, damping=DAMPING):
    """Solved: iterated until the L1 change is below FIXED_POINT_TOLERANCE
    (`iterations` is ignored; it is taken so every convention is called alike)."""
    x = np.full(len(source.nodes), 1.0 / len(source.nodes))
    for following in _normalised_steps(source, damping):
        change, x = np.abs(following - x).sum(), following
        if change < FIXED_POINT_TOLERANCE:
            return _named(source, x)


def unnormalised(source, iterations, damping=DAMPING):
    transposed, out = _matrix(source)
    x = np.full(len(source.nodes), 1 - damping)
    for _ in range(iterations):
        x = (1 - damping) + damping * _spread(transposed, out, x)
    return _named(source, x)


CONVENTIONS = {"normalised": normalised, "fixed point": fixed_point, "unnormalised": unnormalised}
