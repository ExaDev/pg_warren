"""Appendix A's worked use cases (DESIGN.md Appendix A, §13 M11) at about
10M nodes: each module generates its use case's planted structure inside
the database, runs its program in stages and states its expected outcomes
as checks. `bench/appendix_a.py` runs them."""

from . import embedding_propagation, entity_resolution, fraud_rings, ingest_gate, typed_retrieval

CASES = {
    "a1": fraud_rings.use_case,
    "a2": ingest_gate.use_case,
    "a3": entity_resolution.use_case,
    "a4": typed_retrieval.use_case,
    "a5": embedding_propagation.use_case,
}
