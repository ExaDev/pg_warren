"""Neo4j with plain Cypher, no GDS: the workloads one Cypher query can
express, on the store `neo4j-admin import` wrote (each table row a
relationship, parallel edges and an undirected edge's two rows included,
so every query counts distinct nodes). An undirected graph's edges are
followed both ways.

- `degree`: distinct neighbours per node, summed in the server;
- `bfs depth 6`: a variable-length pattern returning distinct nodes, which
  the planner runs as a pruning breadth-first expand;
- `bfs depths`: `shortestPath` from the start to every other node (the
  start is at depth 0); `ANY SHORTEST (s)-->*(n)`, the newer form, took
  more than 600 s on facebook's 4,039 nodes, as did `REPEATABLE ELEMENTS`
  with a bound, so it is not used;
- `shortest paths`: `shortestPath` between each pair, counting hops;
- `triangles`: the undirected simple graph's triangles, as a total.

Timed queries aggregate in the server, so no result crosses Bolt; the
answer is read by a second, untimed query. Each cold run follows a server
restart (an empty page cache, plan cache and JIT); Cypher runs a query on
one thread in Community edition."""

from dataclasses import dataclass

from .neo4j_server import NODE_LABEL, parameters

UNSUPPORTED = {
    "pagerank": "iterative over the whole graph's state: no Cypher query expresses it (GDS or APOC do)",
    "components": "a fixed point of label propagation, or a traversal per component: not one Cypher query",
    "louvain": "a heuristic optimisation over the whole graph: not expressible in Cypher",
    "leiden": "a heuristic optimisation over the whole graph: not expressible in Cypher",
    "betweenness": "needs every pair's shortest paths counted (Brandes): not a practical Cypher query",
}
WEIGHTED_PATHS = "Cypher's shortest paths count hops (shortestPath, SHORTEST); a weighted one needs GDS or APOC"


@dataclass(frozen=True)
class Queries:
    timed: str
    answer: str | None  # None: the timed query's records are the answer


def _arrow(dataset):
    return "->" if dataset.directed else "-"


def queries(workload, dataset):
    """The workload's Cypher, or None when Cypher cannot express it."""
    arrow, node = _arrow(dataset), f"(s:{NODE_LABEL} {{id: $source}})"
    kind = workload.kind
    if kind in UNSUPPORTED or (kind == "paths" and workload.weighted):
        return None
    if kind == "degree":
        each = f"MATCH (n:{NODE_LABEL}) CALL (n) {{ OPTIONAL MATCH (n)-[]{arrow}(m) RETURN count(DISTINCT m) AS d }} "
        return Queries(each + "RETURN sum(d) AS total", each + "RETURN n.id AS id, d AS value")
    if kind == "reach":
        return Queries(f"MATCH {node}-[*0..{workload.depth}]{arrow}(n) RETURN count(DISTINCT n) AS value", None)
    if kind == "depths":
        each = f"MATCH {node} MATCH p = shortestPath((s)-[*]{arrow}(n:{NODE_LABEL})) WHERE n <> s "
        return Queries(each + "RETURN count(n) AS reached, max(length(p)) AS depth",
                       each + "RETURN n.id AS id, length(p) AS value")
    if kind == "paths":
        return Queries(f"UNWIND range(0, size($pairs) - 1) AS i MATCH (s:{NODE_LABEL} {{id: $pairs[i][0]}}), "
                       f"(t:{NODE_LABEL} {{id: $pairs[i][1]}}) OPTIONAL MATCH p = shortestPath((s)-[*]{arrow}(t)) "
                       "RETURN i, length(p) AS value", None)
    # triangles: each unordered pair once, then each third node once.
    return Queries(f"MATCH (a:{NODE_LABEL})--(b:{NODE_LABEL}) WHERE elementId(a) < elementId(b) WITH DISTINCT a, b "
                   f"MATCH (b)--(c:{NODE_LABEL}) WHERE elementId(b) < elementId(c) WITH DISTINCT a, b, c "
                   "WHERE EXISTS { (a)--(c) } RETURN count(*) AS value", None)


class CypherEngine:
    name = "neo4j-cypher"

    def __init__(self, dataset, directory, server, timeout):
        self.dataset, self.directory, self.server, self.timeout = dataset, directory, server, timeout
        self.records = None

    def describe(self):
        return {**self.server.versions(), "threads": 1, "runtime": "Community (slotted, one thread a query)",
                "heap": self.server.heap, "pagecache": self.server.pagecache,
                "store_bytes": self.server.store_bytes()}

    def load(self):
        return self.server.load(self.dataset, self.directory)

    def unsupported(self, workload):
        if workload.kind in UNSUPPORTED:
            return UNSUPPORTED[workload.kind]
        return WEIGHTED_PATHS if workload.kind == "paths" and workload.weighted else None

    def make_cold(self, workload):
        return {"restart": self.server.restart()}

    def reset(self, workload):
        self.records = None

    def run(self, workload):
        self.records = self.server.query(queries(workload, self.dataset).timed, parameters(workload, self.dataset),
                                         self.timeout)

    def answer(self, workload):
        found = queries(workload, self.dataset)
        records = self.records if found.answer is None else self.server.query(
            found.answer, parameters(workload, self.dataset))
        if workload.kind == "reach":
            return records[0]["value"]
        if workload.kind == "triangles":
            return {"total": records[0]["value"]}
        if workload.kind == "paths":
            by_pair = {r["i"]: r["value"] for r in records}
            return {f"{a}->{b}": by_pair.get(i) for i, (a, b) in enumerate(workload.pairs)}
        found = {str(r["id"]): r["value"] for r in records}
        return {workload.source: 0, **found} if workload.kind == "depths" else found

    def close(self):
        self.records = None
