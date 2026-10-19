"""Erdős–Rényi random graphs, G(n, m): `edges` distinct pairs drawn
uniformly over nodes 1..n."""

import numpy as np

from .edge_sampling import distinct_pairs
from .fixture import Fixture, Table


def generate(nodes, edges, seed, directed=False):
    rng = np.random.default_rng(seed)
    pairs = nodes * (nodes - 1) // (1 if directed else 2)
    src, dst = distinct_pairs(
        rng, edges, lambda k: (rng.integers(1, nodes + 1, k), rng.integers(1, nodes + 1, k)),
        directed=directed, limit=pairs)
    return Fixture(
        name="random",
        tables=(
            Table.of("nodes", id=("bigint", np.arange(1, nodes + 1))),
            Table.of("edges", src=("bigint", src), dst=("bigint", dst)),
        ),
        truth={"generator": "random", "nodes": nodes, "edges": edges, "directed": directed, "seed": seed},
    )
