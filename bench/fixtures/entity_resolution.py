"""Entity resolution (Appendix A.3): customer records from several sources
linked to the identifiers they carry.

Each true entity has 1–5 records and one identifier of each kind. A record
of source `s` carries its entity's identifier of kind `k` with probability
`coverage[s][k]`, drawn uniformly from `coverage_range`. Identifier values
never collide by accident: only the planted shared identifiers (a share
`shared_fraction` of all identifiers, each also carried by one record of an
unrelated entity — the shared office phone) link different entities.

Record ids are shuffled so that an entity's records are not contiguous;
identifier ids follow them. The truth holds each record's entity
(`truth_entities`) and the shared identifiers (`truth_shared_identifiers`)."""

import numpy as np

from .fixture import Fixture, Table

KINDS = ("phone", "email", "address", "card")
SOURCES = 3


def generate(records=200_000, entities=60_000, coverage_range=(0.4, 0.9), shared_fraction=0.02, seed=1):
    if not entities <= records <= 5 * entities:
        raise ValueError("every entity needs 1 to 5 records")
    rng = np.random.default_rng(seed)
    extra = rng.choice(4 * entities, records - entities, replace=False) // 4
    entity = np.sort(np.concatenate([np.arange(entities), extra]))
    record_id = rng.permutation(records) + 1
    source = rng.integers(1, SOURCES + 1, records)
    coverage = rng.uniform(*coverage_range, (SOURCES, len(KINDS)))
    carries = rng.random((records, len(KINDS))) < coverage[source - 1]

    rows, kinds = np.nonzero(carries)
    keys = kinds * entities + entity[rows]  # one identifier per (kind, entity)
    used = np.unique(keys)
    identifier_id = records + 1 + np.searchsorted(used, keys)
    owner = used % entities

    shared = np.sort(rng.choice(len(used), round(shared_fraction * len(used)), replace=False))
    sharing_record = rng.integers(0, records, len(shared))
    for i in np.nonzero(entity[sharing_record] == owner[shared])[0]:
        while entity[sharing_record[i]] == owner[shared[i]]:
            sharing_record[i] = rng.integers(0, records)

    link_record = np.concatenate([record_id[rows], record_id[sharing_record]])
    link_identifier = np.concatenate([identifier_id, records + 1 + shared])
    order = np.argsort(record_id)
    kind_of = np.array(KINDS)[used // entities]
    return Fixture("entity-resolution", (
        Table.of("records", id=("bigint", record_id[order]), source=("int", source[order])),
        Table.of("identifiers", id=("bigint", records + 1 + np.arange(len(used))), kind=("text", kind_of.tolist()),
                 value=("text", [_value(k, e) for k, e in zip(kind_of, owner)])),
        Table.of("record_identifiers", record_id=("bigint", link_record), identifier_id=("bigint", link_identifier)),
        Table.of("truth_entities", record_id=("bigint", record_id[order]), entity_id=("bigint", entity[order])),
        Table.of("truth_shared_identifiers", identifier_id=("bigint", records + 1 + shared)),
    ), {
        "generator": "entity-resolution", "records": records, "entities": entities, "sources": SOURCES,
        "kinds": list(KINDS), "coverage": coverage.round(6).tolist(), "identifiers": len(used),
        "shared_identifiers": len(shared), "seed": seed,
    })


def _value(kind, entity):
    return {
        "phone": f"+44 7700 {entity:06d}",
        "email": f"person{entity}@example.org",
        "address": f"{entity} Example Road",
        "card": f"card {entity:06d}",
    }[str(kind)]
