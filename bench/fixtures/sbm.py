"""Stochastic block models (Appendix A.2, A.5): `communities` blocks of
`community_size` nodes, an undirected edge inside a block with probability
`p_in` and across blocks with `p_out`. Community `c` holds ids
`c·size + 1 .. (c+1)·size`. Optionally each node has an embedding drawn
around its community's centroid, and each edge a relation label."""

from dataclasses import dataclass

import numpy as np

from .edge_sampling import distinct_pairs
from .fixture import Fixture, Table


@dataclass(frozen=True)
class BlockModel:
    communities: int
    community_size: int
    p_in: float
    p_out: float
    src: np.ndarray
    dst: np.ndarray
    edges_within: int
    centroids: np.ndarray  # communities × dims, unit rows
    noise: float

    @property
    def nodes(self):
        return self.communities * self.community_size

    def share_inside(self):
        """The share of a node's expected edges that stay in its community."""
        inside = self.p_in * (self.community_size - 1)
        return inside / (inside + self.p_out * (self.nodes - self.community_size))

    def community_of(self, ids):
        return (np.asarray(ids) - 1) // self.community_size

    def embed(self, rng, centroids):
        """One embedding per row of `centroids`: the centroid plus Gaussian
        noise of expected norm `noise`."""
        dims = centroids.shape[1]
        return (centroids + rng.normal(0, self.noise / np.sqrt(dims), centroids.shape)).astype(np.float32)


def build(rng, communities, community_size, p_in, p_out, embedding_dim=0, noise=0.5):
    s = community_size
    src, dst, within = [], [], 0
    for a in range(communities):
        for b in range(a, communities):
            pairs = s * (s - 1) // 2 if a == b else s * s
            count = rng.binomial(pairs, p_in if a == b else p_out)
            x, y = distinct_pairs(
                rng, count, lambda k, a=a, b=b: (a * s + 1 + rng.integers(0, s, k), b * s + 1 + rng.integers(0, s, k)),
                directed=False, limit=pairs)
            src.append(x)
            dst.append(y)
            within += len(x) if a == b else 0
    return BlockModel(communities, s, p_in, p_out, np.concatenate(src), np.concatenate(dst), within,
                      unit_rows(rng.normal(size=(communities, embedding_dim))), noise)


def unit_rows(matrix):
    norms = np.linalg.norm(matrix, axis=1, keepdims=True)
    return matrix / np.where(norms == 0, 1, norms)


def generate(communities, community_size, p_in, p_out, seed, embedding_dim=0, noise=0.5, relations=()):
    rng = np.random.default_rng(seed)
    return fixture(rng, build(rng, communities, community_size, p_in, p_out, embedding_dim, noise), seed, relations)


def fixture(rng, model, seed, relations=()):
    """The tables and truth of `model`; draws relation labels and embeddings from `rng`."""
    ids = np.arange(1, model.nodes + 1)
    embedding_dim = model.centroids.shape[1]
    tables = [
        Table.of("nodes", id=("bigint", ids), community=("int", model.community_of(ids))),
        edges_table("edges", rng, model.src, model.dst, relations),
    ]
    if embedding_dim:
        tables.append(Table.of("embeddings", id=("bigint", ids),
                               embedding=("float4[]", model.embed(rng, model.centroids[model.community_of(ids)]))))
        tables.append(Table.of("truth_centroids", community=("int", np.arange(model.communities)),
                               centroid=("float4[]", model.centroids.astype(np.float32))))
    return Fixture("sbm", tuple(tables), {
        "generator": "sbm", "nodes": model.nodes, "edges": len(model.src),
        "edges_within": model.edges_within, "edges_between": len(model.src) - model.edges_within,
        "communities": model.communities, "community_size": model.community_size,
        "p_in": model.p_in, "p_out": model.p_out, "embedding_dim": embedding_dim, "noise": model.noise,
        "relations": list(relations), "directed": False, "seed": seed,
    })


def edges_table(name, rng, src, dst, relations):
    """`name (src, dst [, rel])`, with `rel` drawn uniformly from `relations`
    when there are any."""
    columns = dict(src=("bigint", src), dst=("bigint", dst))
    if relations:
        columns["rel"] = ("text", rng.choice(list(relations), len(src)).tolist())
    return Table.of(name, **columns)
