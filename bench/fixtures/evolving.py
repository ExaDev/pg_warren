"""Evolving graphs (DESIGN.md §12, Appendix A.2): a stochastic-block-model
base graph plus ingest batches, one per variant, to be inserted into the
base tables inside a transaction:

- *volume*: new entities join existing communities, linking inside their
  own community in the same proportion as the block model does;
- *bridge*: new entities link only into two chosen communities, merging them;
- *novel*: new entities form a new community, linking almost only to each other.

The base tables are those of `sbm.generate` with the same parameters and
seed. Each variant draws from its own seeded stream, so it does not depend on
which other variants are generated."""

import numpy as np

from . import sbm
from .edge_sampling import distinct_pairs
from .fixture import Fixture, Table

VARIANTS = ("volume", "bridge", "novel")
NOVEL_ATTACHMENT = 0.02  # share of a novel entity's edges that reach the base graph


def generate(communities, community_size, p_in, p_out, new_entities, new_edges, seed,
             variants=VARIANTS, embedding_dim=0, noise=0.5):
    rng = np.random.default_rng(seed)
    model = sbm.build(rng, communities, community_size, p_in, p_out, embedding_dim, noise)
    base = sbm.fixture(rng, model, seed)
    batches = [_ingest(model, variant, new_entities, new_edges, seed, embedding_dim) for variant in variants]
    tables = list(base.tables) + [
        Table.of("ingest_nodes", **_stack(batches, "nodes")),
        Table.of("ingest_edges", **_stack(batches, "edges")),
    ]
    if embedding_dim:
        tables.append(Table.of("ingest_embeddings", **_stack(batches, "embeddings")))
    truth = dict(base.truth, generator="evolving", variants={b["variant"]: b["truth"] for b in batches})
    return Fixture("evolving", tuple(tables), truth)


def _ingest(model, variant, entities, edges, seed, embedding_dim):
    rng = np.random.default_rng([seed, 1 + VARIANTS.index(variant)])
    k, s, n = model.communities, model.community_size, model.nodes
    ids = np.arange(n + 1, n + entities + 1)
    truth = {"nodes": entities, "edges": edges}
    centroids = model.centroids

    if variant == "volume":
        community = rng.integers(0, k, entities)
        inside = model.share_inside()

        def draw(count):
            u = rng.integers(0, entities, count)
            own = community[u]
            other = rng.integers(0, n - s, count)
            block = other // s
            block = block + (block >= own)
            v = np.where(rng.random(count) < inside, own * s + rng.integers(0, s, count), block * s + other % s) + 1
            return ids[u], v
    elif variant == "bridge":
        pair = np.sort(rng.choice(k, 2, replace=False))
        community = pair[rng.integers(0, 2, entities)]
        truth["communities"] = pair.tolist()

        def draw(count):
            return ids[rng.integers(0, entities, count)], pair[rng.integers(0, 2, count)] * s + rng.integers(1, s + 1, count)
    elif variant == "novel":
        community = np.full(entities, k)
        centroids = np.vstack([centroids, sbm.unit_rows(rng.normal(size=(1, embedding_dim)))])
        truth["community"] = k

        def draw(count):
            u = ids[rng.integers(0, entities, count)]
            v = np.where(rng.random(count) < NOVEL_ATTACHMENT, rng.integers(1, n + 1, count),
                         ids[rng.integers(0, entities, count)])
            return u, v
    else:
        raise ValueError(f"unknown variant {variant!r}; expected one of {VARIANTS}")

    src, dst = distinct_pairs(rng, edges, draw, directed=False, limit=entities * n + entities * (entities - 1) // 2)
    label = ("text", [variant] * entities)
    batch = {
        "nodes": dict(variant=label, id=("bigint", ids), community=("int", community)),
        "edges": dict(variant=("text", [variant] * edges), src=("bigint", src), dst=("bigint", dst)),
    }
    if embedding_dim:
        batch["embeddings"] = dict(variant=label, id=("bigint", ids),
                                   embedding=("float4[]", model.embed(rng, centroids[community])))
    return dict(batch, variant=variant, truth=truth)


def _stack(batches, table):
    """The columns of `table` across all batches, concatenated in batch order."""
    first = batches[0][table]
    return {name: (sql_type, _concat([b[table][name][1] for b in batches])) for name, (sql_type, _) in first.items()}


def _concat(parts):
    if isinstance(parts[0], list):
        return [v for part in parts for v in part]
    return np.concatenate(parts)
