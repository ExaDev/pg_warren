"""Grid graphs: `rows × cols` cells, cell (r, c) has id `r·cols + c + 1` and
an undirected edge to its right and lower neighbours."""

import numpy as np

from .fixture import Fixture, Table


def generate(rows, cols):
    ids = np.arange(1, rows * cols + 1).reshape(rows, cols)
    src = np.concatenate([ids[:, :-1].ravel(), ids[:-1, :].ravel()])
    dst = np.concatenate([ids[:, 1:].ravel(), ids[1:, :].ravel()])
    return Fixture(
        name="grid",
        tables=(
            Table.of("nodes", id=("bigint", ids.ravel())),
            Table.of("edges", src=("bigint", src), dst=("bigint", dst)),
        ),
        truth={"generator": "grid", "nodes": rows * cols, "edges": len(src), "rows": rows, "cols": cols,
               "directed": False},
    )
