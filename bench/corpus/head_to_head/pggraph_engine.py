"""pgGraph (Evokoa/pgGraph, Apache-2.0; the release `pins.json` names),
through SQL in one backend on the suite's scratch cluster, as pg_warren
runs: the tables loaded by the dataset's `load.sql` (`ingest`), a primary
key added to `edges` (`key`), `nodes` and `edges` registered (`register`:
`graph.add_table`, `graph.add_edge`) and `graph.build()` (`build`, three
times), which writes the persisted artifact. A cold run follows
`graph.unload_graph`, so the next call loads the graph from its artifact
(`graph.auto_load`); a warm one finds it loaded in the backend.

pgGraph reads a relationship table's primary key as each edge's identity,
and the suite's `edges` has none (a parallel edge repeats its `(src,
dst)`), so `key` adds a surrogate `bigint` identity. The cluster runs
pgGraph's recommended starting configuration (`CLUSTER_SETTINGS`: `graph`
preloaded, a 4 GB graph memory limit) with manual sync, its advice for a
static graph, and each session raises the circuit breakers
(`SESSION_SETTINGS`) to their documented maximum, or the memory limit,
so that a whole-graph traversal is not cut short: they bound work, they
do not tune it.

pgGraph's SQL API has traversal (`graph.traverse`), unweighted and
weighted shortest paths and connected components; it has no degree,
PageRank, triangles, community detection or betweenness (`UNSUPPORTED`).
A query runs on its backend's one thread. An undirected graph's edges are
registered `bidirectional` (stored both ways) and a directed one's one
way; traversals follow out-edges, as the reference does. Edges are kept as
rows: parallel edges stay (bag semantics) and a relation is the edge's
dynamic label, which none of the workloads it runs can tell apart from
pg_warren's deduplicated edges.

`graph.traverse` takes a depth bound and accounts for its output as every
visited node's path to that depth, so it has no unbounded search: `bfs
depths` passes the start's eccentricity (`eccentricity`, from the
reference), the smallest bound that reaches every node. pgGraph's edge
weights are integers (`bigint`); a weighted dataset whose weights are not
whole numbers is not timed on paths (`weights_are_integers`)."""

import hashlib
import json
import time
from pathlib import Path

from soak import Session

from .engine import EngineError, Timeout
from .warren_engine import TIMED_OUT

BUILDS = 3
LABEL = "link"  # the static edge label (the fallback when the dataset has relations)
NO_BOUND = 2_147_483_647  # max_rows, and a shortest path's depth bound
KEY_SQL = "ALTER TABLE edges ADD COLUMN edge_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY"
CLUSTER_SETTINGS = (
    "shared_preload_libraries = 'graph'",
    "graph.memory_limit_mb = 4096",
    "graph.maintenance_memory_mb = 4096",
    "graph.sync_mode = 'manual'",
)
SESSION_SETTINGS = (
    "SET graph.max_nodes = 10000000",
    "SET graph.max_frontier = 10000000",
    "SET graph.query_work_limit = 2000000000",
    "SET graph.query_memory_mb = 4096",
)
SETTINGS_SQL = ("SELECT name || '=' || setting FROM pg_settings WHERE name LIKE 'graph.%' "
                "OR name IN ('shared_preload_libraries', 'statement_timeout') ORDER BY 1")
UNSUPPORTED = {
    "degree": "no degree function in pgGraph's SQL API, and graph.gql's subset rejects a per-node count with OPTIONAL MATCH",
    "pagerank": "no PageRank in pgGraph's SQL API",
    "triangles": "no triangle count in pgGraph's SQL API",
    "louvain": "no community detection in pgGraph's SQL API",
    "leiden": "no community detection in pgGraph's SQL API",
    "betweenness": "no betweenness in pgGraph's SQL API",
}
FRACTIONAL_WEIGHTS = "pgGraph's edge weights are bigint, and this dataset's weights are not all whole numbers"


def quoted(text):
    return "'" + text.replace("'", "''") + "'"


def register_sql(dataset):
    """`nodes` and `edges` registered as pgGraph's node table and edge-table
    relationship."""
    weight = ", weight_column := 'weight'" if dataset.weighted else ""
    label = ", label_column := 'rel'" if dataset.relations else ""
    return ("SELECT graph.add_table('nodes'::regclass, 'id'); "
            f"SELECT graph.add_edge('edges'::regclass, 'src', 'nodes'::regclass, 'dst', '{LABEL}', "
            f"bidirectional := {'false' if dataset.directed else 'true'}{weight}{label})")


def traverse_sql(start, depth, columns):
    return (f"SELECT {columns} FROM graph.traverse('nodes'::regclass, {quoted(start)}, {depth}, "
            f"direction := 'out', hydrate := false, max_rows := {NO_BOUND})")


def paths_sql(workload):
    pairs = ", ".join(f"({i}, {quoted(a)}, {quoted(b)})" for i, (a, b) in enumerate(workload.pairs))
    measure = ("(SELECT max(total_cost) FROM graph.weighted_shortest_path('nodes'::regclass, p.s, "
               "'nodes'::regclass, p.t))" if workload.weighted else
               f"(SELECT max(step) FROM graph.shortest_path('nodes'::regclass, p.s, 'nodes'::regclass, p.t, "
               f"{NO_BOUND}, false))")
    return f"SELECT json_agg({measure} ORDER BY p.i)::text FROM (VALUES {pairs}) p (i, s, t)"


def run_sql(workload, depth_bound=None):
    """What a run executes; `depth_bound` is the depths workload's."""
    kind = workload.kind
    if kind == "reach":
        return traverse_sql(workload.source, workload.depth, "count(*)")
    if kind == "depths":
        return traverse_sql(workload.source, depth_bound, "count(*), max(depth)")
    if kind == "paths":
        return paths_sql(workload)
    return "SELECT count(DISTINCT component_id) FROM graph.connected_components()"


def cluster_settings(settings, engines):
    """The cluster's `settings`, with pgGraph's when it is one of `engines`
    (pg_warren and pgGraph share the cluster)."""
    return (*settings, *CLUSTER_SETTINGS) if "pggraph" in engines else tuple(settings)


def pin():
    """The pgGraph release and build `pins.json` records."""
    return json.loads((Path(__file__).resolve().parent / "pins.json").read_text())["pggraph"]


def library(pkglibdir):
    """The `graph` library the cluster loads, and its sha256."""
    path = Path(pkglibdir) / "graph.so"
    return {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}


def weights_are_integers(directory):
    with open(directory / "edges.csv", encoding="utf-8") as f:
        return all(float(line.rsplit(",", 1)[1]).is_integer() for line in f)


class PgGraphEngine:
    """`eccentricity(start)` is the largest hop count from `start` to a node
    it reaches (the reference's), the depth bound of `bfs depths`."""

    name = "pggraph"

    def __init__(self, cluster, dataset, directory, timeout, library, pin, eccentricity):
        self.cluster, self.dataset, self.directory, self.timeout = cluster, dataset, directory, timeout
        self.library, self.pin, self.eccentricity = library, pin, eccentricity
        self.database_name = "h2h_pggraph_" + dataset.name.replace("-", "_")
        self.session = self.rows = self.graph_name = None
        self.integer_weights = not dataset.weighted or weights_are_integers(directory)
        self.bounds = {}

    def _run(self, sql):
        ran = self.session.run(sql)
        if ran["errors"]:
            message = " ".join(ran["errors"])
            raise (Timeout if TIMED_OUT in message else EngineError)(message[:500])
        return ran

    def describe(self):
        status = json.loads(self._run("SELECT row_to_json(s)::text FROM graph.status() s")["rows"][0])
        return {"version": self._run("SELECT extversion FROM pg_extension WHERE extname = 'graph'")["rows"][0],
                "threads": 1, "pin": self.pin, "library": self.library,
                "settings": self._run(SETTINGS_SQL)["rows"], "session": list(SESSION_SETTINGS),
                "depth_bounds": self.bounds,
                "graph": {key: status.get(key) for key in ("node_count", "edge_count", "memory_used_mb")}}

    def load(self):
        admin = self.cluster.database()
        admin.execute(f'DROP DATABASE IF EXISTS "{self.database_name}"')
        admin.execute(f'CREATE DATABASE "{self.database_name}"')
        database = self.cluster.database(self.database_name)
        database.execute("CREATE EXTENSION graph")
        started = time.perf_counter()
        database.run_file_checked(self.directory / "load.sql", cwd=self.directory)
        ingest = time.perf_counter() - started
        self.session = Session(self.cluster, self.database_name)
        for setting in SESSION_SETTINGS:
            self._run(setting)
        self._run(f"SET statement_timeout = '{self.timeout}s'")
        key = self._run(KEY_SQL)["seconds"]
        register = self._run(register_sql(self.dataset))["seconds"]
        builds = [self._run("SELECT edges_loaded FROM graph.build()")["seconds"] for _ in range(BUILDS)]
        self.graph_name = self._run("SELECT graph_name FROM graph.current_graph()")["rows"][0]
        return {"ingest": [ingest], "key": [key], "register": [register], "build": builds}

    def unsupported(self, workload):
        if workload.kind == "paths" and workload.weighted and not self.integer_weights:
            return FRACTIONAL_WEIGHTS
        return UNSUPPORTED.get(workload.kind)

    def _bound(self, workload):
        if workload.source not in self.bounds:
            self.bounds[workload.source] = self.eccentricity(workload.source)
        return self.bounds[workload.source]

    def make_cold(self, workload):
        if workload.kind == "depths":
            self._bound(workload)
        return {"unload": self._run(f"SELECT unloaded FROM graph.unload_graph({quoted(self.graph_name)})")["seconds"]}

    def reset(self, workload):
        self.rows = None

    def run(self, workload):
        bound = self._bound(workload) if workload.kind == "depths" else None
        self.rows = self._run(run_sql(workload, bound))["rows"]

    def _pairs(self, sql):
        return {str(node): value for node, value in (json.loads(row) for row in self._run(sql)["rows"])}

    def answer(self, workload):
        kind = workload.kind
        if kind == "reach":
            return int(self.rows[0])
        if kind == "paths":
            return {f"{a}->{b}": found for (a, b), found in zip(workload.pairs, json.loads(self.rows[0]))}
        if kind == "depths":
            return self._pairs(traverse_sql(workload.source, self._bound(workload),
                                            "json_build_array(node_id, depth)::text"))
        return self._pairs("SELECT json_build_array(node_id, component_id)::text FROM graph.connected_components()")

    def close(self):
        if self.session is not None:
            self.session.__exit__()
            self.session = None
        self.cluster.database().execute(f'DROP DATABASE IF EXISTS "{self.database_name}"')
