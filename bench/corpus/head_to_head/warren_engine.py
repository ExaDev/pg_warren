"""pg_warren, through SQL in one backend (the soak's `Session`, a psql
coprocess), on the suite's scratch cluster: the tables loaded by the
dataset's `load.sql` (`ingest`: COPY and ANALYZE), then `warren.refresh`
(`build`, three times). A cold run follows `warren.evict`, so it loads
the version from its segments; a warm one finds it in the backend's
cache.

The library's PageRank, Louvain and Leiden and `degree_vector` persist
their output vector (they have no `persist` argument); `triangles` and
`betweenness_centrality` run with `persist := false`. Traversals return
their rows to the server's count; the answers are read afterwards as JSON
rows, untimed."""

import json
import time

from soak import SETTINGS_SQL, Session

from .engine import EngineError, Timeout

GRAPH = "g"
VECTORS = {"degree": "h2h_degree", "pagerank": "h2h_pagerank", "triangles": "h2h_triangles",
           "louvain": "h2h_louvain", "leiden": "h2h_leiden", "betweenness": "h2h_betweenness"}
BUILDS = 3
TIMED_OUT = "canceling statement due to statement timeout"


def literal(node, id_type):
    if id_type == "bigint":
        return f"{node}::bigint"
    return "'" + node.replace("'", "''") + "'::text"


def paths_sql(workload, id_type):
    pairs = ", ".join(f"({i}, {literal(a, id_type)}, {literal(b, id_type)})" for i, (a, b) in
                      enumerate(workload.pairs))
    measure = (f"(SELECT cost FROM warren.dijkstra('{GRAPH}', p.s, p.t))" if workload.weighted else
               f"array_length(warren.shortest_path('{GRAPH}', p.s, p.t), 1) - 1")
    return f"SELECT json_agg({measure} ORDER BY p.i)::text FROM (VALUES {pairs}) p (i, s, t)"


def run_sql(workload, id_type):
    """What a run executes."""
    kind, vector = workload.kind, VECTORS.get(workload.kind)
    start = None if workload.source is None else f"ARRAY[{literal(workload.source, id_type)}]"
    return {
        "degree": lambda: f"SELECT warren.degree_vector('{GRAPH}', '{vector}', 'out')",
        "reach": lambda: f"SELECT count(*) FROM warren.bfs('{GRAPH}', {start}, {workload.depth})",
        "depths": lambda: f"SELECT count(*), max(depth) FROM warren.bfs('{GRAPH}', {start}, NULL)",
        "paths": lambda: paths_sql(workload, id_type),
        "pagerank": lambda: (f"SELECT warren.pagerank('{GRAPH}', '{vector}', tol := 0, "
                             f"max_iter := {workload.iterations})"),
        "components": lambda: f"SELECT count(DISTINCT component_id) FROM warren.components('{GRAPH}', NULL::{id_type})",
        "triangles": lambda: f"SELECT warren.triangles('{GRAPH}', '{vector}', persist := false)",
        "louvain": lambda: f"SELECT warren.louvain('{GRAPH}', '{vector}')",
        "leiden": lambda: f"SELECT warren.leiden('{GRAPH}', '{vector}')",
        "betweenness": lambda: f"SELECT warren.betweenness_centrality('{GRAPH}', '{vector}', persist := false)",
    }[kind]()


class WarrenEngine:
    name = "pg_warren"

    def __init__(self, cluster, dataset, directory, create_graph_sql, threads, timeout):
        self.cluster, self.dataset, self.directory = cluster, dataset, directory
        self.create_graph_sql, self.threads, self.timeout = create_graph_sql, threads, timeout
        self.id_type = "bigint" if dataset.ids == "integer" else "text"
        self.database_name = "h2h_" + dataset.name.replace("-", "_")
        self.session = self.rows = None

    def _run(self, sql):
        ran = self.session.run(sql)
        if ran["errors"]:
            message = " ".join(ran["errors"])
            raise (Timeout if TIMED_OUT in message else EngineError)(message[:500])
        return ran

    def describe(self):
        info = json.loads(self._run(f"SELECT warren.info('{GRAPH}')")["rows"][0])
        version = next(v for v in info["versions"] if v["current"])
        return {"version": self._run("SELECT warren.version()")["rows"][0], "threads": self.threads,
                "settings": self._run(SETTINGS_SQL)["rows"],
                "graph": {key: version.get(key) for key in ("node_count", "edge_count", "dropped_edges", "bytes")}}

    def load(self):
        admin = self.cluster.database()
        admin.execute(f'DROP DATABASE IF EXISTS "{self.database_name}"')
        admin.execute(f'CREATE DATABASE "{self.database_name}"')
        database = self.cluster.database(self.database_name)
        database.execute("CREATE EXTENSION pg_warren")
        started = time.perf_counter()
        database.run_file_checked(self.directory / "load.sql", cwd=self.directory)
        ingest = time.perf_counter() - started
        self.session = Session(self.cluster, self.database_name)
        self._run(self.create_graph_sql)
        self._run(f"SET warren.threads = {self.threads}")
        self._run(f"SET statement_timeout = '{self.timeout}s'")
        builds = [self._run(f"SELECT warren.refresh('{GRAPH}')")["seconds"] for _ in range(BUILDS)]
        return {"ingest": [ingest], "build": builds}

    def unsupported(self, workload):
        return None

    def make_cold(self, workload):
        return {"evict": self._run(f"SELECT warren.evict('{GRAPH}')")["seconds"]}

    def reset(self, workload):
        self.rows = None
        if workload.kind in VECTORS:
            self._run(f"SELECT warren.drop_vector('{GRAPH}', '{VECTORS[workload.kind]}', NULL, true)")

    def run(self, workload):
        self.rows = self._run(run_sql(workload, self.id_type))["rows"]

    def _pairs(self, sql):
        return {str(node): value for node, value in (json.loads(row) for row in self._run(sql)["rows"])}

    def _vector(self, workload):
        return self._pairs(f"SELECT json_build_array(node_id, value)::text FROM warren.vector_values('{GRAPH}', "
                           f"'{VECTORS[workload.kind]}', NULL::{self.id_type})")

    def answer(self, workload):
        kind = workload.kind
        if kind == "reach":
            return int(self.rows[0])
        if kind == "paths":
            return {f"{a}->{b}": found for (a, b), found in zip(workload.pairs, json.loads(self.rows[0]))}
        if kind == "depths":
            return self._pairs(f"SELECT json_build_array(node_id, depth)::text FROM warren.bfs('{GRAPH}', "
                               f"ARRAY[{literal(workload.source, self.id_type)}], NULL)")
        if kind == "components":
            return self._pairs(f"SELECT json_build_array(node_id, component_id)::text FROM "
                               f"warren.components('{GRAPH}', NULL::{self.id_type})")
        values = self._vector(workload)
        if kind == "pagerank":
            return "normalised", values
        return values if kind == "betweenness" else {node: int(value) for node, value in values.items()}

    def close(self):
        if self.session is not None:
            self.session.__exit__()
            self.session = None
        self.cluster.database().execute(f'DROP DATABASE IF EXISTS "{self.database_name}"')
