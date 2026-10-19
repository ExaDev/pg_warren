"""Edge sampling: drawing a given number of distinct node pairs."""

import numpy as np


def distinct_pairs(rng, count, draw, *, directed, limit):
    """Returns `count` distinct pairs as two int64 arrays, in the order they
    were first drawn. `draw(k)` returns `k` candidate pairs; self-loops are
    dropped and, unless `directed`, a pair is stored as (smaller, larger).
    `limit` is how many distinct pairs exist, to refuse impossible requests."""
    if count > limit:
        raise ValueError(f"asked for {count} distinct pairs but only {limit} exist")
    src = np.empty(0, dtype=np.int64)
    dst = np.empty(0, dtype=np.int64)
    while len(src) < count:
        a, b = (np.asarray(x, dtype=np.int64) for x in draw(int((count - len(src)) * 1.1) + 16))
        if not directed:
            a, b = np.minimum(a, b), np.maximum(a, b)
        keep = a != b
        src, dst = np.concatenate([src, a[keep]]), np.concatenate([dst, b[keep]])
        _, first = np.unique(np.stack([src, dst]), axis=1, return_index=True)
        first.sort()
        src, dst = src[first], dst[first]
    return src[:count], dst[:count]
