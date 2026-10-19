"""Power-law graphs by preferential attachment (Barabási–Albert): nodes
1..m start the graph, and each later node links to `attachments` distinct
earlier nodes chosen with probability proportional to their degree."""

import numpy as np

from .fixture import Fixture, Table


def generate(nodes, attachments, seed):
    rng = np.random.default_rng(seed)
    m = attachments
    # One entry per edge end (plus one per seed node): uniform sampling from
    # it is sampling proportional to degree.
    ends = list(range(1, m + 1))
    src, dst = [], []
    for node in range(m + 1, nodes + 1):
        targets = set()
        while len(targets) < m:
            targets.update(ends[i] for i in (rng.random(m - len(targets)) * len(ends)).astype(int))
        for target in sorted(targets):
            src.append(node)
            dst.append(target)
            ends.extend((node, target))
    return Fixture(
        name="power-law",
        tables=(
            Table.of("nodes", id=("bigint", np.arange(1, nodes + 1))),
            Table.of("edges", src=("bigint", np.array(src, dtype=np.int64)),
                     dst=("bigint", np.array(dst, dtype=np.int64))),
        ),
        truth={"generator": "power-law", "nodes": nodes, "edges": len(src), "attachments": m,
               "directed": False, "seed": seed},
    )
