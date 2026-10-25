"""What must hold once a fuzz run's sessions have disconnected: no
backend left in the activity table, nothing pinned or mapped in the
shared cache, a fresh backend holding no memory, the canary unwritten
and the source tables and secret intact (nothing the caller's SQL did
escaped its read-only transaction), the catalog consistent with the
segments (every version's bytes are its segments', every current version
a promoted version of its graph, every version loads and every
persisted vector reads), and, once every graph is dropped through the
API, nothing left in the catalog."""

from ..concurrent_refresh import until
from ..session import Session
from .fixture import BIGINT_IDS, EDGE_ROWS, INTEGER_IDS, SECRET, SECRET_ROWS, STORED_IDS, TEXT_IDS, UUID_IDS

SOURCES = {"public.nodes8": len(BIGINT_IDS), "public.nodes4": len(INTEGER_IDS), "public.nodes_text": len(TEXT_IDS),
           "public.nodes_uuid": len(UUID_IDS), **EDGE_ROWS, SECRET: SECRET_ROWS, "public.canary": 0,
           STORED_IDS: 2}
CATALOG = ("warren.graphs", "warren.versions", "warren.segments", "warren.vectors", "warren.vector_chunks",
           "warren.vector_quotas")

UNSOUND_VERSIONS = """
SELECT v.version_id FROM warren.versions v
WHERE v.bytes <> (SELECT coalesce(sum(length(s.data)), 0) FROM warren.segments s WHERE s.version_id = v.version_id)
"""
UNSOUND_CURRENT = """
SELECT g.name FROM warren.graphs g
WHERE g.current_version IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM warren.versions v
    WHERE v.version_id = g.current_version AND v.graph_id = g.graph_id AND NOT v.staged)
"""


def settled(cluster, database):
    """What is wrong in `database` after a run (see the module's doc comment)."""
    failures = []
    with Session(cluster, database) as s:
        if until(s, "SELECT count(*) FROM warren.backends() WHERE pid <> pg_backend_pid()") != "0":
            failures.append(f"backends left in warren.backends(): {s.run('SELECT * FROM warren.backends()').rows}")
        if until(s, "SELECT count(*) FROM warren.shared_cache() WHERE pins <> 0 OR mappers <> 0") != "0":
            failures.append(f"shared entries still pinned or mapped: {s.run('SELECT * FROM warren.shared_cache()').rows}")
        own = s.run("SELECT private_bytes + transient_bytes FROM warren.backends() WHERE pid = pg_backend_pid()")
        if own.rows != [["0"]]:
            failures.append(f"a fresh backend's private memory is {own.rows}, not 0")
        for table, rows in SOURCES.items():
            count = s.run(f"SELECT count(*) FROM {table}")
            if count.rows != [[str(rows)]]:
                failures.append(f"{table} holds {count.rows or count.message}, not {rows} rows")
        failures += _catalog(s)
        failures += _dropped(s)
    return failures


def _catalog(s):
    failures = []
    for sql, what in ((UNSOUND_VERSIONS, "versions whose segments do not add up to their bytes"),
                      (UNSOUND_CURRENT, "graphs whose current version is not a promoted version of theirs")):
        rows = s.run(sql).rows
        if rows:
            failures.append(f"{what}: {rows}")
    # By id, the names read in the same statement: a name may hold a tab or
    # a newline, which psql's rows would split.
    for (version,) in s.run("SELECT version_id FROM warren.versions ORDER BY version_id").rows:
        loaded = s.run(f"SELECT warren.prewarm(g.name, v.version_id) FROM warren.versions v "
                       f"JOIN warren.graphs g USING (graph_id) WHERE v.version_id = {version}")
        if not loaded.ok:
            failures.append(f"version {version} does not load: {loaded.sqlstate} {loaded.message}")
    for (vector,) in s.run("SELECT vector_id FROM warren.vectors ORDER BY vector_id").rows:
        read = s.run(f"SELECT warren.vector_stats(g.name, x.name, x.version_id) FROM warren.vectors x "
                     f"JOIN warren.graphs g USING (graph_id) WHERE x.vector_id = {vector}")
        if not read.ok:
            failures.append(f"vector {vector} does not read: {read.sqlstate} {read.message}")
    return failures


def _dropped(s):
    failures = []
    for (graph,) in s.run("SELECT graph_id FROM warren.graphs ORDER BY graph_id").rows:
        dropped = s.run(f"SELECT warren.drop_graph(name) FROM warren.graphs WHERE graph_id = {graph}")
        if not dropped.ok:
            failures.append(f"dropping graph {graph} failed: {dropped.sqlstate} {dropped.message}")
    for table in CATALOG:
        left = s.run(f"SELECT count(*) FROM {table}").value()
        if left != "0":
            failures.append(f"{left} rows left in {table} after every graph was dropped")
    orphans = s.run("SELECT * FROM warren.orphans()").rows
    if orphans:
        failures.append(f"orphans after every graph was dropped: {orphans}")
    return failures
