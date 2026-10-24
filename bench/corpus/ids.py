"""Text node ids measured for the front-coding decision (DESIGN.md §5.7,
§14 item 41): the distribution of their UTF-8 byte lengths, the sizes of
the id map's three arrays as a text version stores them (`id_bytes`,
`id_offsets` at u64 a node plus one, `id_order` at u32), and what front
coding would store instead: the ids sorted bytewise in blocks of 16, each
block's first id whole and every other as the length of the prefix it
shares with its predecessor, its suffix's length and the suffix (varints),
plus a u64 offset a block. Front coding also needs a permutation from
dense id to sorted rank (`id_order`'s inverse, u32 a node), not counted
in its bytes."""

import math
from collections import Counter

BLOCK = 16
PERCENTILES = (50, 90, 99)


def distribution(ids):
    lengths = sorted(len(i.encode("utf-8")) for i in ids)
    count = len(lengths)
    return {"count": count, "bytes": sum(lengths), "min": lengths[0], "max": lengths[-1],
            "mean": sum(lengths) / count,
            "percentiles": {str(p): lengths[math.ceil(p / 100 * count) - 1] for p in PERCENTILES},
            "lengths": {str(k): v for k, v in sorted(Counter(lengths).items())}}


def _varint(n):
    return max(1, math.ceil(n.bit_length() / 7))


def _shared(a, b):
    n = 0
    for x, y in zip(a, b):
        if x != y:
            break
        n += 1
    return n


def front_coding(ids, block=BLOCK):
    encoded = sorted(i.encode("utf-8") for i in ids)
    total, shared = 0, []
    for start in range(0, len(encoded), block):
        first = encoded[start]
        total += _varint(len(first)) + len(first)
        for previous, current in zip(encoded[start:start + block], encoded[start + 1:start + block]):
            common = _shared(previous, current)
            suffix = len(current) - common
            total += _varint(common) + _varint(suffix) + suffix
            shared.append(common)
    blocks = math.ceil(len(encoded) / block)
    return {"bytes": total, "blocks": blocks, "offsets_bytes": 8 * blocks,
            "mean_shared_prefix": sum(shared) / len(shared) if shared else 0.0}


def report(ids, nodes):
    """`ids` (the graph's node ids) measured: their distribution, the id
    map's arrays for `nodes` nodes, and the front-coded size beside them."""
    ids = list(ids)
    found = distribution(ids)
    coded = front_coding(ids)
    id_map = {"id_bytes": found["bytes"], "id_offsets": 8 * (nodes + 1), "id_order": 4 * nodes}
    return {"distribution": found, "id_map": {**id_map, "total": sum(id_map.values())},
            "front_coded": {**coded, "ratio": found["bytes"] / coded["bytes"],
                            "ratio_with_offsets": found["bytes"] / (coded["bytes"] + coded["offsets_bytes"]),
                            "rank_permutation_bytes": 4 * nodes}}
