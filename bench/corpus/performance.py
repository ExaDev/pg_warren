"""The §12 performance matrix on a corpus dataset: the build (`refresh`),
a cold load, BFS to depth 6 against the recursive CTE, `reachable_set`,
`components`, `pagerank` (20 iterations), `eigen` (k = 20, normalised
Laplacian, smallest), `aggregate_neighbours` (d = 64, as `bench/matrix.json`
has it; §12's d = 768 needs memory these graphs do not justify) and `diff`
against a second version with 0.1% of the edges removed, plus the
library's structural and community calls. Each stage runs in one backend
through the soak's `Session` (`bench/soak.py`), which records its seconds
and its peak private memory (RssAnon) from /proc; short reads are also
timed inside the server and repeated."""

import json
import statistics
from dataclasses import dataclass

from matrix import TIMER
from soak import SETTINGS_SQL, Session, server_seconds, timed

STATEMENT_TIMEOUT = "10min"  # eigen's §12 target; every stage gets the same bound
EIGEN_TOL = 1e-6
DEPTH = 6
REMOVED = "hashtext(src::text || ' ' || dst::text) % 1000 = 7"  # 0.1% of edges, for the second version

@dataclass(frozen=True)
class Stage:
    name: str
    sql: str
    repeat: int = 1
    server: bool = False  # timed inside the backend (TIMER), for latencies a round trip would blur


def edges_per_second(edges, seconds):
    return edges / seconds


def cte_sql(dataset, source):
    """BFS to depth 6 as a recursive CTE over the edge table (indexed on
    src and dst), the §12 baseline: out-edges of a directed graph, both
    directions of an undirected one."""
    step = "SELECT dst FROM edges WHERE src = r.id" + ("" if dataset.directed else
                                                        " UNION ALL SELECT src FROM edges WHERE dst = r.id")
    return (f"WITH RECURSIVE r (id, depth) AS (SELECT {source}, 0 UNION SELECT n.id, r.depth + 1 FROM r "
            f"CROSS JOIN LATERAL ({step}) n (id) WHERE r.depth < {DEPTH}) SELECT count(DISTINCT id) FROM r")


def stages(dataset, source):
    """The read stages, from a cold cache, for start node `source` (an SQL
    literal of the graph's id type)."""
    id_type = "bigint" if dataset.ids == "integer" else "text"
    embedding = ("SELECT id, (SELECT array_agg(sin(k * (hashtext(id::text) % 1000))::real ORDER BY k) "
                 "FROM generate_series(1, 64) k) FROM nodes")
    return [
        Stage("cold load", "SELECT warren.evict('g'), warren.prewarm('g')"),
        Stage("bfs depth 6", f"SELECT count(*) FROM warren.bfs('g', ARRAY[{source}], {DEPTH})", 3, True),
        Stage("bfs depth 6 recursive CTE", cte_sql(dataset, source)),
        Stage("neighbours", f"SELECT count(*) FROM warren.neighbours('g', {source})", 20, True),
        Stage("reachable_set", f"SELECT count(*) FROM warren.reachable_set('g', ARRAY[{source}])", 3, True),
        Stage("components", f"SELECT count(DISTINCT component_id) FROM warren.components('g', NULL::{id_type})"),
        Stage("pagerank 20 iterations", "SELECT warren.pagerank('g', 'pr', tol := 0, max_iter := 20)"),
        Stage("eigen k=20", "SELECT count(*), max(residual) FROM warren.eigen('g', 20, 'normalized_laplacian', "
                            f"'smallest', tol := {EIGEN_TOL})"),
        Stage("embedding d=64 load", f"SELECT warren.vector_from_query('g', 'emb', $q${embedding}$q$, 'f32')"),
        Stage("aggregate_neighbours d=64", "SELECT warren.aggregate_neighbours('g', 'emb', 'emb_nbr')"),
        Stage("triangles", "SELECT warren.triangles('g', 'tri')"),
        Stage("clustering", "SELECT warren.clustering('g', 'cc')"),
        Stage("core_number", "SELECT warren.core_number('g', 'core')"),
        Stage("louvain", "SELECT warren.louvain('g', 'louvain')"),
        Stage("leiden", "SELECT warren.leiden('g', 'leiden')"),
    ]


def _hub(database, dataset):
    """The node with most edges (ties to the smallest id), as an SQL literal."""
    ends = "SELECT src FROM edges UNION ALL SELECT dst FROM edges"
    return database.query(f"SELECT quote_literal(n) || '::' || pg_typeof(n) FROM ({ends}) e (n) "
                          "GROUP BY n ORDER BY count(*) DESC, n LIMIT 1")[0][0]


def _version(database):
    info = json.loads(database.query("SELECT warren.info('g')")[0][0])
    return next(v for v in info["versions"] if v["current"])


def run(cluster, name, database, dataset, threads, create_graph_sql):
    """Builds graph `g` of database `name` and runs the matrix; returns the
    versions, each stage's measurement and the settings it ran under."""
    database.execute(TIMER)
    database.execute("CREATE INDEX ON edges (src); CREATE INDEX ON edges (dst); ANALYZE edges")
    database.execute(create_graph_sql)
    source = _hub(database, dataset)
    result = {"source": source, "stages": {}}
    with Session(cluster, name) as s:
        s.run(f"SET warren.threads = {threads}")
        s.run(f"SET statement_timeout = '{STATEMENT_TIMEOUT}'")
        result["stages"]["refresh"] = s.measure("SELECT warren.refresh('g')")
        result["version"] = _version(database)
        for stage in stages(dataset, source):
            measured = s.measure(timed(stage.sql) if stage.server else stage.sql, stage.repeat)
            result["stages"][stage.name] = server_seconds(measured) if stage.server else measured
            if stage.name == "reachable_set":
                result["stages"]["reachable_set stats"] = json.loads(s.run("SELECT warren.last_stats()")["rows"][0])
        result["settings"] = s.run(SETTINGS_SQL)["rows"]
        v1 = result["version"]["version"]
        s.run(f"DELETE FROM edges WHERE {REMOVED}")
        result["stages"]["second refresh"] = s.measure("SELECT warren.refresh('g')")
        result["stages"]["diff"] = s.measure(f"SELECT warren.diff('g', {v1}, {_version(database)['version']})")
    reached = result["stages"]["reachable_set"]
    if "server_median_seconds" in reached:
        result["reachable_set_edges_per_second"] = edges_per_second(
            result["version"]["edge_count"], statistics.median(reached["server_seconds"]))
    return result
