"""pg_warren beside a primary it is not installed on (DESIGN.md §13, 0.9.0
"Running beside RDS today"; docs/beside-managed.md): the source rows the
primary starts with and the changes made to them, waiting until the graph
server has caught up, and the comparisons of what it answers with a graph
built directly from the primary's rows.

The guide's blocks run as written (harness/guide.py). What a managed
primary provides and a scratch cluster does not is stood in for, and only
that: the `rds_replication` role, which RDS gives the replication
attribute (here the role is created, and the attribute set on the
guide's replication role), and the application's role `app`. The graph
server's `pg_hba.conf` asks the results' reader for its password, as
`postgres_fdw` needs of a non-superuser's connection."""

import math
import random
import time

from . import server_log

PRIMARY_DATABASE = "app"
GRAPH_DATABASE = "graph"
REFERENCE_DATABASE = "reference"
SLOT = "graph_source"
WAIT_SECONDS = 60

PRIMARY_SETTINGS = ("wal_level = logical",)
PASSWORDS = {"replica_password": "replica-secret", "graph_reader_password": "reader-secret",
             "source_password": "source-secret"}
# What a managed primary has that a scratch cluster lacks (see above).
STAND_INS = "CREATE ROLE rds_replication; CREATE ROLE app LOGIN"
REPLICATION_ATTRIBUTE = "ALTER ROLE warren_replica REPLICATION"
READER_HBA = f"local {GRAPH_DATABASE} graph_reader scram-sha-256\n"

PEOPLE = 400
FOLLOWS = 2000
SEED = 1
STARTS = (1, 2, 3, 50, 199, 400)
DEPTH = 4
PAGERANK_TOLERANCE = 1e-12

# The SQLSTATEs of the server logs that are expected: none.
ALLOWED = frozenset()


def variables(primary, graph):
    """The guide's psql variables for the scratch servers: their sockets
    (`sslmode` is ignored on a Unix socket, so the guide's `verify-full`
    stands) and the roles' passwords."""
    conninfo = f"host={primary.directory} port=5432 dbname={PRIMARY_DATABASE} user=warren_replica sslmode=verify-full"
    return {**PASSWORDS, "primary": conninfo, "graph_host": str(graph.directory), "primary_host": str(primary.directory)}


def people(count, start=1):
    return [(i, f"person-{i}") for i in range(start, start + count)]


def follows(rng, ids, count, existing=frozenset()):
    """`count` edges between `ids` drawn by `rng`, none a loop, none in `existing`."""
    edges = set()
    while len(edges) < count:
        src, dst = rng.choice(ids), rng.choice(ids)
        if src != dst and (src, dst) not in existing:
            edges.add((src, dst))
    return sorted(edges)


def initial_rows():
    """The primary's first rows: people, and edges among them."""
    rng = random.Random(SEED)
    nodes = people(PEOPLE)
    return nodes, follows(rng, [i for i, _ in nodes], FOLLOWS)


def changes(edges):
    """Statements that change the primary in several transactions, as an
    application would: new people with edges to and from them (one
    transaction, so a version never has those edges without their nodes),
    edges deleted, edges moved to another target, and people deleted
    with their edges (one transaction)."""
    rng = random.Random(SEED + 1)
    added = people(40, start=PEOPLE + 1)
    ids = list(range(1, PEOPLE + 41))
    new_edges = follows(rng, ids, 300, existing=set(edges))
    gone = rng.sample(edges, 100)
    kept = sorted(set(edges) - set(gone) - set(new_edges))
    moved = rng.sample(kept, 20)
    taken = set(edges) | set(new_edges)
    retargeted = []
    for src, dst in moved:
        target = next(t for t in rng.sample(ids, len(ids)) if t != src and (src, t) not in taken)
        taken.add((src, target))
        retargeted.append((src, dst, target))
    removed = rng.sample([i for i in range(1, PEOPLE + 1) if i not in STARTS], 10)
    return [
        "BEGIN; " + values_insert("people (id, name)", added) + " " + values_insert("follows (src, dst)", new_edges)
        + " COMMIT",
        "DELETE FROM follows WHERE (src, dst) IN (" + ", ".join(f"({s}, {d})" for s, d in gone) + ")",
        "BEGIN; " + " ".join(f"UPDATE follows SET dst = {t} WHERE src = {s} AND dst = {d};" for s, d, t in retargeted)
        + " COMMIT",
        f"BEGIN; DELETE FROM follows WHERE src IN ({ids_list(removed)}) OR dst IN ({ids_list(removed)}); "
        f"DELETE FROM people WHERE id IN ({ids_list(removed)}); COMMIT",
    ]


def values_insert(table, rows):
    return f"INSERT INTO {table} VALUES " + ", ".join("(" + ", ".join(sql_literal(v) for v in row) + ")"
                                                     for row in rows) + ";"


def sql_literal(value):
    return str(value) if isinstance(value, int) else "'" + str(value).replace("'", "''") + "'"


def ids_list(ids):
    return ", ".join(map(str, sorted(ids)))


def wait_until(check, what, seconds=WAIT_SECONDS):
    """Waits until `check()` is true; `RuntimeError` naming `what` if it is
    not in `seconds`."""
    deadline = time.monotonic() + seconds
    while not check():
        if time.monotonic() > deadline:
            raise RuntimeError(f"{what} did not happen in {seconds} s")
        time.sleep(0.1)


def caught_up(primary_db, lsn):
    """Whether the graph server has confirmed the primary's WAL up to `lsn`."""
    rows = primary_db.query(f"SELECT confirmed_flush_lsn >= '{lsn}'::pg_lsn FROM pg_replication_slots "
                            f"WHERE slot_name = '{SLOT}'")
    return rows == [["t"]]


TABLES = {"people": "SELECT id, name FROM people ORDER BY id",
          "follows": "SELECT src, dst FROM follows ORDER BY src, dst"}


def table_differences(primary_db, graph_db):
    """The replicated tables whose rows differ between the two servers."""
    return [f"{table} differs: {len(primary_db.query(sql))} rows on the primary, "
            f"{len(graph_db.query(sql))} on the graph server"
            for table, sql in TABLES.items() if primary_db.query(sql) != graph_db.query(sql)]


def load_reference(primary_db, reference_db):
    """Rebuilds the reference graph `social` from the primary's rows as
    they are now, copied into ordinary tables on the graph server."""
    reference_db.execute("TRUNCATE people, follows")
    reference_db.copy_in("people", primary_db.query(TABLES["people"]))
    reference_db.copy_in("follows", primary_db.query(TABLES["follows"]))
    reference_db.execute("SELECT warren.refresh('social')")


REFERENCE_SETUP = ("CREATE EXTENSION pg_warren; "
                   "CREATE TABLE people (id bigint PRIMARY KEY, name text NOT NULL); "
                   "CREATE TABLE follows (src bigint NOT NULL, dst bigint NOT NULL, PRIMARY KEY (src, dst)); "
                   "SELECT warren.create_graph('social', 'SELECT id FROM people', 'SELECT src, dst FROM follows')")


def traversals(db, graph):
    """What graph `graph` answers that does not depend on the order of its
    dense ids: its node and edge counts, each start's breadth-first
    levels, and the hop count of a shortest path between starts."""
    answers = {"counts": db.query(f"SELECT warren.info('{graph}') ->> 'node_count', "
                                  f"warren.info('{graph}') ->> 'edge_count'")}
    for start in STARTS:
        answers[f"bfs from {start}"] = db.query(
            f"SELECT node_id, depth FROM warren.bfs('{graph}', ARRAY[{start}::bigint], {DEPTH}) ORDER BY node_id")
    for start, target in zip(STARTS, STARTS[1:]):
        answers[f"path {start} to {target}"] = db.query(
            f"SELECT cardinality(warren.shortest_path('{graph}', {start}::bigint, {target}::bigint))")
    return answers


def traversal_differences(expected, found, name):
    return [f"{name}: {key} is {short(found.get(key))}, the reference's {short(value)}"
            for key, value in expected.items() if found.get(key) != value]


def pagerank(db, graph):
    """Graph `graph`'s PageRank by node, computed and dropped."""
    db.execute(f"SELECT warren.pagerank('{graph}', 'reference_pr')")
    ranks = db.query(f"SELECT node_id, value FROM warren.vector_values('{graph}', 'reference_pr', NULL::bigint)")
    db.execute(f"SELECT warren.drop_vector('{graph}', 'reference_pr')")
    return ranks_by_node(ranks)


def ranks_by_node(rows):
    return {int(node): float(value) for node, value in rows}


def rank_differences(expected, found, name, tolerance=PAGERANK_TOLERANCE):
    """Where ranks `found` (node to rank) differ from `expected`: a node
    missing or extra, or a rank off by more than `tolerance`. The order of
    dense ids changes the order of the sums, so the last bits may differ."""
    problems = [f"{name}: nodes {sorted(set(expected) ^ set(found))[:10]} are not in both"] \
        if set(expected) != set(found) else []
    off = [node for node in sorted(set(expected) & set(found))
           if not math.isclose(expected[node], found[node], rel_tol=0, abs_tol=tolerance)]
    if off:
        node = off[0]
        problems.append(f"{name}: {len(off)} ranks differ, node {node}: {found[node]} against {expected[node]}")
    return problems


def top_ranks_differences(top, ranks, count=10):
    """Whether `top`, rows of (id, score) from the guide's query of the
    ten highest ranks, are `count` of the highest of `ranks`."""
    highest = sorted(ranks.values(), reverse=True)[:count]
    scores = [float(score) for _, score in top]
    problems = [] if scores == highest else [f"the ten highest ranks read {scores}, not {highest}"]
    problems += [f"node {node} read a rank of {score}, not {ranks.get(int(node))}"
                 for node, score in top if ranks.get(int(node)) != float(score)]
    return problems


# Queries of the foreign table whose remote SQL must carry the clause:
# what the guide says runs on the graph server.
PUSHED_DOWN = {
    "SELECT id, score FROM graph_results.rank ORDER BY score DESC LIMIT 10": ("ORDER BY score DESC", "LIMIT 10"),
    "SELECT score FROM graph_results.rank WHERE id = 42": ("WHERE ((id = 42))",),
}


def remote_sql(plan):
    """The `Remote SQL` lines of an `EXPLAIN VERBOSE` plan, given as rows."""
    lines = [row[0].strip() for row in plan]
    return [line.removeprefix("Remote SQL: ") for line in lines if line.startswith("Remote SQL: ")]


def pushdown_differences(sql, plan, clauses):
    sent = remote_sql(plan)
    return [f"{sql}: the remote SQL {sent} lacks {clause!r}" for clause in clauses
            if not any(clause in line for line in sent)]


def log_problems(text):
    return [f"server log: {line}" for line in server_log.problems(text, ALLOWED)]


def short(value, limit=200):
    text = repr(value)
    return text if len(text) <= limit else text[:limit] + "..."
