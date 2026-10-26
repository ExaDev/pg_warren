"""Neo4j with the Graph Data Science library (Community edition, so at most
4 threads an algorithm): the store `neo4j-admin import` wrote (shared with
the Cypher engine), projected into GDS's in-memory graph as pg_warren
builds its CSR: every relationship type into one, parallel relationships
aggregated into one (`SINGLE`, their weights summed), an undirected graph
projected `UNDIRECTED`. A directed graph also gets an undirected
projection, `u`, for the algorithms GDS runs only on undirected graphs
(triangles, Leiden) and for Louvain, whose modularity pg_warren takes in
the undirected view.

Each algorithm runs in `mutate` mode, its result kept in the projection as
a node property (pg_warren's library keeps a vector), and is read back
untimed. Paths and BFS reach stream, aggregated in the server; single-source
depths are mutated as relationships from the start. A cold run
follows a server restart and new projections (recorded as `cold_phases`),
so it meets a cold JVM as a first call would."""

import time

from .neo4j_server import NODE_LABEL, parameters

UNDIRECTED_ONLY = ("triangles", "louvain", "leiden")
PROPERTIES = {"degree": "degree", "pagerank": "pagerank", "components": "components", "triangles": "triangles",
              "louvain": "louvain", "leiden": "leiden", "betweenness": "betweenness"}
LEIDEN_SEED = 1
DEPTHS = "depths"  # the relationship type the single-source depths are mutated into


def graph_for(workload, dataset):
    """The projection a workload reads: `u` for those of the undirected
    view on a directed graph, else `g`."""
    return "u" if dataset.directed and workload.kind in UNDIRECTED_ONLY else "g"


def _weight(dataset):
    return ", relationshipWeightProperty: 'weight'" if dataset.weighted else ""


def project_call(name, dataset, orientation):
    weight = ", properties: {weight: {property: 'weight', aggregation: 'SUM'}}" if dataset.weighted else ""
    return (f"CALL gds.graph.project('{name}', '{NODE_LABEL}', {{ALL: {{type: '*', orientation: '{orientation}', "
            f"aggregation: 'SINGLE'{weight}}}}}) YIELD relationshipCount RETURN relationshipCount")


def mutate_call(workload, dataset, threads):
    """The `mutate` call of an algorithm's workload."""
    kind, graph = workload.kind, graph_for(workload, dataset)
    common = f"mutateProperty: '{PROPERTIES[kind]}', concurrency: {threads}"
    procedure, options = {
        "degree": ("gds.degree", ""),
        # GDS counts its initial vector as the first iteration.
        "pagerank": ("gds.pageRank", f", maxIterations: {(workload.iterations or 0) + 1}, tolerance: 0.0, "
                                     f"dampingFactor: 0.85{_weight(dataset)}"),
        "components": ("gds.wcc", ""),
        "triangles": ("gds.triangleCount", ""),
        "louvain": ("gds.louvain", _weight(dataset)),
        "leiden": ("gds.leiden", f", randomSeed: {LEIDEN_SEED}{_weight(dataset)}"),
        "betweenness": ("gds.betweenness", _weight(dataset)),
    }[kind]
    return f"CALL {procedure}.mutate('{graph}', {{{common}{options}}}) YIELD nodePropertiesWritten RETURN 1"


def stream_call(workload, dataset, threads):
    """The streamed workloads, aggregated in the server: (timed, answer)."""
    start = f"MATCH (s:{NODE_LABEL} {{id: $source}}) "
    if workload.kind == "reach":
        return (start + f"CALL gds.bfs.stream('g', {{sourceNode: s, maxDepth: {workload.depth}, "
                f"concurrency: {threads}}}) YIELD nodeIds RETURN size(nodeIds) AS value", None)
    if workload.kind == "depths":
        # Mutated as relationships from the start: streaming holds every
        # target's path (roadNet-TX's 1.35M ran out of a 4 GB heap).
        return (start + f"CALL gds.allShortestPaths.delta.mutate('g', {{sourceNode: s, concurrency: {threads}, "
                f"mutateRelationshipType: '{DEPTHS}'}}) YIELD relationshipsWritten RETURN relationshipsWritten",
                f"CALL gds.graph.relationshipProperty.stream('g', 'totalCost', ['{DEPTHS}']) "
                "YIELD targetNodeId, propertyValue RETURN gds.util.asNode(targetNodeId).id AS id, propertyValue AS value")
    weight = _weight(dataset) if workload.weighted else ""
    return (f"UNWIND range(0, size($pairs) - 1) AS i MATCH (s:{NODE_LABEL} {{id: $pairs[i][0]}}), "
            f"(t:{NODE_LABEL} {{id: $pairs[i][1]}}) CALL gds.shortestPath.dijkstra.stream('g', "
            f"{{sourceNode: s, targetNode: t, concurrency: {threads}{weight}}}) YIELD totalCost "
            "RETURN i, totalCost AS value", None)


def betweenness_scale(nodes, directed):
    """GDS sums betweenness over ordered pairs of a directed graph and
    unordered pairs of an undirected one; NetworkX normalises by the
    ordered pairs of the other nodes."""
    return (1 if directed else 2) / ((nodes - 1) * (nodes - 2)) if nodes > 2 else 1


class GdsEngine:
    name = "neo4j-gds"

    def __init__(self, dataset, directory, server, timeout, threads):
        self.dataset, self.directory, self.server = dataset, directory, server
        self.timeout, self.threads = timeout, threads
        self.records = None

    def describe(self):
        return {**self.server.versions(), "threads": self.threads, "edition": "community, no licence key",
                "mode": "mutate", "heap": self.server.heap, "pagecache": self.server.pagecache}

    def _project(self):
        phases = {}
        orientations = [("g", "NATURAL" if self.dataset.directed else "UNDIRECTED")]
        orientations += [("u", "UNDIRECTED")] if self.dataset.directed else []
        for name, orientation in orientations:
            started = time.perf_counter()
            self.server.query(project_call(name, self.dataset, orientation))
            phases["project" if name == "g" else "project undirected"] = time.perf_counter() - started
        return phases

    def load(self):
        loaded = self.server.load(self.dataset, self.directory)
        self.server.query("CALL gds.graph.list() YIELD graphName CALL gds.graph.drop(graphName) YIELD nodeCount "
                          "RETURN count(*)")
        return {**loaded, **{phase: [seconds] for phase, seconds in self._project().items()}}

    def unsupported(self, workload):
        return None

    def make_cold(self, workload):
        return {"restart": self.server.restart(), **self._project()}

    def reset(self, workload):
        self.records = None
        if workload.kind == "depths":
            self.server.query("CALL gds.graph.list('g') YIELD schemaWithOrientation "
                              f"WHERE schemaWithOrientation.relationships['{DEPTHS}'] IS NOT NULL "
                              f"CALL gds.graph.relationships.drop('g', '{DEPTHS}') YIELD graphName RETURN 1")
        if workload.kind in PROPERTIES:
            self.server.query(f"CALL gds.graph.nodeProperties.drop('{graph_for(workload, self.dataset)}', "
                              f"['{PROPERTIES[workload.kind]}'], {{failIfMissing: false}}) YIELD propertiesRemoved "
                              "RETURN 1")

    def run(self, workload):
        cypher = (mutate_call(workload, self.dataset, self.threads) if workload.kind in PROPERTIES
                  else stream_call(workload, self.dataset, self.threads)[0])
        self.records = self.server.query(cypher, parameters(workload, self.dataset), self.timeout)

    def answer(self, workload):
        kind = workload.kind
        if kind in PROPERTIES:
            records = self.server.query(
                f"CALL gds.graph.nodeProperty.stream('{graph_for(workload, self.dataset)}', '{PROPERTIES[kind]}') "
                "YIELD nodeId, propertyValue RETURN gds.util.asNode(nodeId).id AS id, propertyValue AS value")
            values = {str(r["id"]): r["value"] for r in records}
            return self._named(workload, values)
        _, answer = stream_call(workload, self.dataset, self.threads)
        records = self.records if answer is None else self.server.query(answer, parameters(workload, self.dataset))
        if kind == "reach":
            return records[0]["value"]
        if kind == "paths":
            by_pair = {r["i"]: r["value"] for r in records}
            return {f"{a}->{b}": None if by_pair.get(i) is None else (
                by_pair[i] if workload.weighted else int(by_pair[i])) for i, (a, b) in enumerate(workload.pairs)}
        return {workload.source: 0, **{str(r["id"]): int(r["value"]) for r in records}}

    def _named(self, workload, values):
        if workload.kind == "pagerank":
            return "unnormalised", values
        if workload.kind == "betweenness":
            scale = betweenness_scale(len(values), self.dataset.directed)
            return {node: value * scale for node, value in values.items()}
        return {node: int(value) for node, value in values.items()}

    def close(self):
        self.records = None
