"""Physical streaming replicas (DESIGN.md §13, M11 "Robustness"; §5.1, §5.6,
§7).

Graphs, versions and vectors are ordinary WAL-logged tables (§5.1), so a
physical standby replays them, and its backends load them into their own
caches. A standby is read-only, in recovery: every transaction there is
read-only, and none can be assigned a transaction id.

The check builds graphs on a primary (`setup_sql`: `bigint` and `text`
ids, weighted, several relations; staged, promoted, dropped and compacted
versions; persisted, dropped and moved vectors; a contraction; owners,
readers and a quota), waits for a standby, and a standby of the standby
(cascading), to replay them (`wait_for_replay`), and then checks there:

- every read (`READS`) answers exactly as on the primary, including the
  compute that persists nothing (transient vectors from the constructors,
  `spmv`, `vector_map`, the kernels, `project` and `lift`; `gather`;
  `eigen` without stored eigenvectors), and works for a reader but not
  for a stranger; reads of the server's own state (`SERVER_STATE`) work;
- every write (`WRITES`) is refused with `read_only_sql_transaction`
  (25006) and pg_warren's message (`REFUSAL`), never an internal error:
  no function assigns a transaction id on a standby;
- the shared cache and the activity table work: two backends share a
  version, `warren.backends()` and `warren.progress()` list them, and
  `evict` leaves nothing pinned (`shared_cache_works`);
- a version retired on the primary while a standby transaction reads it
  (`Retirement`) is either still read consistently or the standby's
  query is cancelled by the recovery conflict (40001), and a new
  statement after the replay no longer finds it (42704);
- after the standby is promoted, it writes: refreshes, persists vectors,
  and its cascading standby follows it.

The server logs hold nothing but what these explain (`ALLOWED`)."""

import threading
import time

from . import server_log, shared_cache
from .session import BackendLost, Session

REPLAY_SECONDS = 60

# Every server's settings (harness.cluster turns the first two off): commits
# wait for their WAL to be flushed, so that the flush location read after a
# commit covers it, on the primary and on a promoted standby; full-page
# writes, without which a standby refuses to be backed up (for the
# cascade); and on a standby, a replayed cleanup that conflicts with a
# query waits at most 2 s before cancelling it (the default 30 s would
# hold the check).
SETTINGS = ("synchronous_commit = on", "full_page_writes = on", "max_standby_streaming_delay = '2s'")

REFUSAL = ("25006", "cannot change pg_warren's catalog in a read-only transaction")

# What the server logs may hold, by SQLSTATE: refused writes (25006); the
# stranger's reads (42501); a retired version named after the replay
# (42704); a standby query cancelled, or its idle transaction's connection
# ended, by a recovery conflict (40001), and the statements of that
# transaction after it (25P02); walreceivers and connections ended when a
# server stops or a standby is promoted (57P01).
ALLOWED = {"25006", "42501", "42704", "40001", "25P02", "57P01"}

OWNER, READER, STRANGER = "warren_owner", "warren_reader", "warren_stranger"


def setup_sql():
    """The roles, source tables and graphs, built and changed on the primary:
    `social` (`bigint` ids, directed, weighted, relations `follows` and
    `likes`) with a staged version promoted, one dropped, tombstones
    compacted away and vectors moved by the compaction; `kg` (`text` ids,
    undirected, relations `knows` and `cites`) with a Louvain labelling and
    its contraction `kg_rings`; `lineage`, a DAG; and `churn`, which keeps
    one version, for `Retirement`."""
    return f"""
CREATE ROLE {OWNER}; CREATE ROLE {READER}; CREATE ROLE {STRANGER};
CREATE TABLE public.people AS SELECT g::bigint AS id FROM generate_series(1, 400) g;
CREATE TABLE public.ties AS
    SELECT g::bigint AS src, (g % 400 + 1)::bigint AS dst, 'follows' AS rel, (1 + g % 7)::float8 AS w
    FROM generate_series(1, 400) g
    UNION ALL SELECT g, (g * 37) % 400 + 1, 'likes', (1 + g % 3)::float8 / 2 FROM generate_series(1, 400) g
    UNION ALL SELECT g, (g * 11) % 400 + 1, 'follows', 2.5 FROM generate_series(1, 400, 3) g;
CREATE TABLE public.terms AS SELECT 'n' || g AS id FROM generate_series(1, 300) g;
CREATE TABLE public.links AS
    SELECT 'n' || g AS src, 'n' || (g % 300 + 1) AS dst, 'knows' AS rel FROM generate_series(1, 300) g
    WHERE g % 30 <> 0
    UNION ALL SELECT 'n' || g, 'n' || ((g / 30) * 30 + (g * 7) % 30 + 1), 'cites' FROM generate_series(1, 299) g;
CREATE TABLE public.churn_edges AS SELECT g::bigint AS src, (g % 50 + 1)::bigint AS dst FROM generate_series(1, 50) g;
GRANT SELECT ON public.people, public.ties, public.terms, public.links, public.churn_edges TO {OWNER};

SELECT warren.create_graph('social', 'SELECT id FROM public.people', 'SELECT src, dst, rel, w FROM public.ties',
                           weighted := true, has_relations := true, retain_versions := 3);
SELECT warren.set_owner('social', '{OWNER}');
SELECT warren.grant('social', '{READER}');
SELECT warren.refresh('social');
SELECT warren.vector_const('social', 'scratch', 2);
SELECT warren.drop_vector('social', 'scratch');
SELECT warren.set_vector_quota('social', 1000000, '{READER}');
DELETE FROM public.people WHERE id > 380;
SELECT warren.refresh('social', promote := false, label := 'tombstones');
SELECT warren.promote('social', (SELECT max(version_id) FROM warren.versions v JOIN warren.graphs g USING (graph_id)
                                 WHERE g.name = 'social'));
SELECT warren.drop_version('social', warren.refresh('social', promote := false));
SELECT warren.vector_const('social', 'ones', 1);
SELECT warren.pagerank('social', 'rank');
SELECT warren.compact('social');
DELETE FROM public.ties WHERE src % 50 = 0;
SELECT warren.refresh('social');
SELECT warren.vector_const('social', 'ones', 1);
SELECT warren.pagerank('social', 'rank');
SELECT warren.vector_from_query('social', 'features', 'SELECT id, ARRAY[id::float8, (id % 5)::float8] FROM public.people');
SELECT warren.spmv('social', 'ones', 'spread');
SELECT * FROM warren.eigen('social', 2, 'laplacian', 'smallest', vectors_prefix := 'lap');

SELECT warren.create_graph('kg', 'SELECT id FROM public.terms', 'SELECT src, dst, rel FROM public.links',
                           directed := false, has_relations := true);
SELECT warren.refresh('kg');
SELECT warren.louvain('kg', 'community');
SELECT warren.contract('kg', 'community', 'kg_rings');
SELECT warren.vector_const('kg', 'one', 1);
SELECT warren.project('kg', 'one', 'kg_rings', 'ring_size');

SELECT warren.create_graph('lineage', 'SELECT generate_series(1, 60)::bigint',
                           'SELECT g::bigint, (g + 1)::bigint FROM generate_series(1, 59) g
                            UNION ALL SELECT g, 2 * g FROM generate_series(1, 30) g');
SELECT warren.refresh('lineage');

SELECT warren.create_graph('churn', 'SELECT DISTINCT src FROM public.churn_edges',
                           'SELECT src, dst FROM public.churn_edges', retain_versions := 1);
SELECT warren.refresh('churn');
"""


# Versions the reads name, read on the primary: social's compacted version
# and the one refreshed after it, and kg's current version.
VERSIONS_SQL = (
    "SELECT (SELECT max(version_id) FROM warren.versions WHERE compacted_from IS NOT NULL), "
    "(warren.info('social') ->> 'current_version'), (warren.info('kg') ->> 'current_version')")


def versions(row):
    """The names `READS` use for the versions `VERSIONS_SQL` read."""
    return dict(zip(("compacted", "current", "kg"), row))


# Reads: each a list of statements run in one session, whose outcomes
# (rows, or SQLSTATE) must be the same on the primary and the standby.
# `{compacted}`, `{current}` and `{kg}` are version ids (`versions`).
# `info`'s staleness is time since the build, so it is left out.
READS = {
    "bfs": ["SELECT * FROM warren.bfs('social', ARRAY[1::bigint], 4, rels := ARRAY['follows']) ORDER BY 1, 2, 3"],
    "bfs by version": ["SELECT * FROM warren.bfs('social', ARRAY[3::bigint], 3, direction := 'both', "
                       "version := {compacted}) ORDER BY 1, 2, 3"],
    "neighbours": ["SELECT n FROM warren.neighbours('social', 7::bigint, direction := 'both') n ORDER BY 1"],
    "neighbours, text ids": ["SELECT n FROM warren.neighbours('kg', 'n42'::text) n ORDER BY 1"],
    "reachable_set": ["SELECT count(*), sum(n) FROM warren.reachable_set('social', ARRAY[1::bigint], 6) n"],
    "shortest_path": ["SELECT warren.shortest_path('kg', 'n1'::text, 'n150'::text)"],
    "dijkstra": ["SELECT * FROM warren.dijkstra('social', 1::bigint, 200::bigint)"],
    "walk": ["SELECT end_id, count FROM warren.walk('social', ARRAY[1::bigint, 2::bigint], "
             "ARRAY[warren.s(ARRAY['follows'], 'out'), warren.s(ARRAY['likes'], 'out')], mode := 'count') "
             "ORDER BY 1"],
    "components": ["SELECT component_id, count(*) FROM warren.components('kg', NULL::text) GROUP BY 1 ORDER BY 1"],
    "degree and has_edge": ["SELECT warren.degree('social', 5::bigint, direction := 'both'), "
                            "warren.has_edge('social', 1::bigint, 2::bigint, 'follows'), "
                            "warren.reachable('kg', 'n1'::text, 'n299'::text)"],
    "id_map": ["SELECT count(*), min(m::text), max(m::text) FROM warren.id_map('kg', NULL::text) m"],
    "info": ["SELECT regexp_replace(warren.info(g)::text, '\"staleness\": \"[^\"]*\"', '', 'g') "
             "FROM unnest(ARRAY['social', 'kg', 'kg_rings', 'churn']) g"],
    "vector_values": ["SELECT * FROM warren.vector_values('social', 'rank', NULL::bigint) ORDER BY 1"],
    "vector_topk": ["SELECT * FROM warren.vector_topk('social', 'spread', 10, NULL::bigint) ORDER BY 2 DESC, 1"],
    "matrix_rows": ["SELECT * FROM warren.matrix_rows('social', 'features', NULL::bigint) ORDER BY 1 LIMIT 50"],
    "eigenvectors": ["SELECT * FROM warren.vector_values('social', 'lap_2', NULL::bigint) ORDER BY 1"],
    "vector_stats": ["SELECT warren.vector_stats('social', 'rank'), warren.vector_reduce('social', 'ones', 'sum')"],
    "modularity": ["SELECT warren.modularity('kg', 'community')"],
    "most_similar": ["SELECT * FROM warren.most_similar('kg', 'n10'::text, 5) ORDER BY 2 DESC, 1"],
    "diff": ["SELECT warren.diff('social', {compacted}, {current})"],
    "distance": ["SELECT warren.distance('social', {compacted}, {current}, 'edit'), "
                 "warren.distance('social', {compacted}, {current}, 'degree')"],
    "vector_usage": ["SELECT * FROM warren.vector_usage('social') ORDER BY 1"],
    "transient spmv": ["SELECT warren.spmv('social', 'ones', 'mine', transpose := true, persist := false)",
                       "SELECT warren.vector_map('social', 'mine2', 'mine * 2', persist := false)",
                       "SELECT * FROM warren.vector_values('social', 'mine2', NULL::bigint) ORDER BY 1"],
    "transient vector_from_query": [
        "SELECT warren.vector_from_query('kg', 'picked', 'SELECT id, length(id) FROM public.terms', "
        "persist := false)",
        "SELECT * FROM warren.vector_topk('kg', 'picked', 5, NULL::text) ORDER BY 2 DESC, 1"],
    "gather": ["SELECT node_id, value FROM warren.gather('kg', 'SELECT id, 1 FROM public.terms', NULL::text, "
               "NULL::bigint, combine := 'sum') ORDER BY 1"],
    "eigen, not stored": ["SELECT i, round(eigenvalue::numeric, 4) FROM warren.eigen('kg', 2, 'laplacian', "
                          "'smallest', dtype := 'f64') ORDER BY 1"],
    "eigen, transient": ["SELECT count(*) FROM warren.eigen('kg', 2, 'laplacian', 'smallest', dtype := 'f64', "
                         "vectors_prefix := 'mylap', persist := false)",
                         "SELECT count(*) FROM warren.vector_values('kg', 'mylap_1', NULL::text)"],
    "contraction": ["SELECT count(*) FROM warren.bfs('kg_rings', ARRAY[(SELECT min(node_id) FROM "
                    "warren.id_map('kg_rings', NULL::bigint))], 3)"],
    "explain_map and vector_norm_diff": ["SELECT warren.explain_map('social', 'rank * 2'), "
                                         "warren.vector_norm_diff('social', 'rank', 'ones')"],
    "minimum_spanning_forest": ["SELECT * FROM warren.minimum_spanning_forest('social', NULL::bigint) "
                                "ORDER BY 1, 2 LIMIT 40"],
    "similar_pairs and similarity": [
        "SELECT * FROM warren.similar_pairs('kg', NULL::text, 5) ORDER BY 3 DESC, 1, 2",
        "SELECT * FROM warren.similarity('kg', $$SELECT 'n1', 'n2'$$, NULL::text)"],
    "steps, s and version": ["SELECT warren.steps('[]'), warren.s(NULL), warren.version()"],
    "orphans": ["SELECT * FROM warren.orphans()"],
    "queries_from_keys": ["SELECT * FROM warren.queries_from_keys(ARRAY['pg_catalog.pg_namespace']::regclass[])"],
    "transient kernels": [
        "SELECT warren.core_number('kg', 'k_core', persist := false), "
        "warren.strong_components_vector('social', 'k_scc', persist := false), "
        "warren.topological_order('lineage', 'k_topo', persist := false), "
        "warren.vector_degree('social', 'k_degree', persist := false), "
        "warren.vector_dense_id('kg', 'k_dense', persist := false), "
        "warren.local_moving('kg', 'k_moved', persist := false), "
        "warren.leiden_refine('kg', 'community', 'k_refined', persist := false), "
        "warren.harmonic_centrality('kg', 'k_harmonic', persist := false), "
        "warren.closeness_centrality('kg', 'k_closeness', persist := false), "
        "warren.betweenness_centrality('kg', 'k_betweenness', persist := false), "
        "warren.triangles('kg', 'k_triangles', persist := false), "
        "warren.dag_longest_path('lineage', 'k_longest', persist := false)",
        "SELECT n, warren.vector_reduce('kg', n, 'sum') FROM unnest(ARRAY['k_core', 'k_dense', 'k_moved', "
        "'k_refined', 'k_harmonic', 'k_closeness', 'k_betweenness', 'k_triangles']) n",
        "SELECT n, warren.vector_reduce('social', n, 'sum') FROM unnest(ARRAY['k_scc', 'k_degree']) n",
        "SELECT n, warren.vector_reduce('lineage', n, 'sum') FROM unnest(ARRAY['k_topo', 'k_longest']) n"],
    "transient project and lift": [
        "SELECT warren.project('kg', 'k_core', 'kg_rings', 'k_ring_core', persist := false)",
        "SELECT warren.lift('kg_rings', 'k_ring_core', 'kg', 'k_lifted', persist := false)",
        "SELECT warren.vector_reduce('kg', 'k_lifted', 'sum')"],
    "as a reader": [f"SET ROLE {READER}",
                    "SELECT count(*) FROM warren.reachable_set('social', ARRAY[1::bigint], 3)", "RESET ROLE"],
    "as a stranger": [f"SET ROLE {STRANGER}",
                      "SELECT count(*) FROM warren.reachable_set('social', ARRAY[1::bigint], 3)", "RESET ROLE"],
}

# Reads of the server's own state: they must work on a standby, where they
# answer for the standby's backends, not the primary's.
SERVER_STATE = ["SELECT warren.prewarm('kg')", "SELECT count(*) > 0 FROM warren.cache()",
                "SELECT count(*) FROM warren.shared_cache()", "SELECT count(*) > 0 FROM warren.backends()",
                "SELECT warren.report_progress('standby check', 'kg', 'reading')",
                "SELECT count(*) FROM warren.progress()", "SELECT count(*) FROM warren.neighbours('kg', 'n1'::text)",
                "SELECT warren.last_stats() IS NOT NULL", "SELECT warren.evict('kg')"]

# Functions no statement calls: the catalog's triggers, which only writes
# fire, and the event trigger, which only DDL fires; a standby runs
# neither.
NOT_CALLED = ("current_version_is_kept", "current_version_is_the_graphs", "label_vector_is_forgotten",
              "on_drop_owned")

# Writes: refused on a standby, each with `REFUSAL`. `{current}` is
# social's current version.
WRITES = {
    "create_graph": "SELECT warren.create_graph('new', 'SELECT 1::bigint', 'SELECT 1::bigint, 1::bigint')",
    "create_graph_from_keys": "SELECT warren.create_graph_from_keys('new', ARRAY['pg_catalog.pg_namespace']::regclass[])",
    "refresh": "SELECT warren.refresh('social')",
    "refresh, staged": "SELECT warren.refresh('kg', promote := false)",
    "promote": "SELECT warren.promote('social', {current})",
    "drop_version": "SELECT warren.drop_version('social', {compacted})",
    "compact": "SELECT warren.compact('social')",
    "drop_graph": "SELECT warren.drop_graph('churn')",
    "grant": f"SELECT warren.grant('social', '{STRANGER}')",
    "revoke": f"SELECT warren.revoke('social', '{READER}')",
    "set_owner": f"SELECT warren.set_owner('kg', '{OWNER}')",
    "set_vector_quota": f"SELECT warren.set_vector_quota('social', 0, '{READER}')",
    "vector_const": "SELECT warren.vector_const('social', 'more', 1)",
    "vector_from_query": "SELECT warren.vector_from_query('social', 'more', 'SELECT id, 1 FROM public.people')",
    "vector_map": "SELECT warren.vector_map('social', 'more', 'ones + 1')",
    "spmv": "SELECT warren.spmv('social', 'ones', 'more')",
    "eigen, stored": "SELECT * FROM warren.eigen('kg', 1, vectors_prefix := 'more')",
    "triangles": "SELECT warren.triangles('kg', 'more')",
    "a transient write over a persisted name": "SELECT warren.vector_const('social', 'rank', 1, persist := false)",
    "drop_vector": "SELECT warren.drop_vector('social', 'rank')",
    "pagerank": "SELECT warren.pagerank('social', 'more')",
    "components_vector": "SELECT warren.components_vector('kg', 'more')",
    "aggregate_neighbours": "SELECT warren.aggregate_neighbours('social', 'features', 'more')",
    "clustering": "SELECT warren.clustering('kg', 'more')",
    "degree_vector": "SELECT warren.degree_vector('social', 'more')",
    "eigenvector_centrality": "SELECT warren.eigenvector_centrality('social', 'more', tol := 1)",
    "hits": "SELECT warren.hits('social', 'more', 'more_too')",
    "katz_centrality": "SELECT warren.katz_centrality('social', 'more')",
    "label_propagation": "SELECT warren.label_propagation('kg', 'more')",
    "label_propagation_majority": "SELECT warren.label_propagation_majority('lineage', 'more')",
    "sssp_vector": "SELECT warren.sssp_vector('social', 'more', 1::bigint)",
    "leiden": "SELECT warren.leiden('kg', 'more')",
    "spmm": "SELECT warren.spmm('social', 'features', 'more')",
    "vector_degree": "SELECT warren.vector_degree('social', 'more')",
    "vector_dense_id": "SELECT warren.vector_dense_id('social', 'more')",
    "core_number": "SELECT warren.core_number('kg', 'more')",
    "strong_components_vector": "SELECT warren.strong_components_vector('social', 'more')",
    "topological_order": "SELECT warren.topological_order('lineage', 'more')",
    "dag_longest_path": "SELECT warren.dag_longest_path('lineage', 'more')",
    "local_moving": "SELECT warren.local_moving('kg', 'more')",
    "leiden_refine": "SELECT warren.leiden_refine('kg', 'community', 'more')",
    "harmonic_centrality": "SELECT warren.harmonic_centrality('kg', 'more')",
    "closeness_centrality": "SELECT warren.closeness_centrality('kg', 'more')",
    "betweenness_centrality": "SELECT warren.betweenness_centrality('kg', 'more')",
    "lift": "SELECT warren.lift('kg_rings', 'ring_size', 'kg', 'more')",
    "louvain": "SELECT warren.louvain('kg', 'more')",
    "contract": "SELECT warren.contract('kg', 'community', 'more_rings')",
    "project": "SELECT warren.project('kg', 'community', 'kg_rings', 'more')",
    "reassign_owned": f"SELECT warren.reassign_owned('{OWNER}', '{READER}')",
    "drop_owned": f"SELECT warren.drop_owned('{READER}')",
    "drop_orphans": "SELECT warren.drop_orphans()",
}


def outcomes(s, statements, names):
    """What each of `statements` gave in session `s`, `names` filled in:
    rows, or a SQLSTATE and message."""
    found = []
    for statement in statements:
        done = s.run(statement.format(**names))
        found.append(done.rows if done.ok else (done.sqlstate, done.message))
    return found


def compare_reads(expected, s, names):
    """What differs between `expected` (the primary's `READS` outcomes) and
    session `s`'s, and any read that failed on the primary but should not."""
    failures = []
    for read, statements in READS.items():
        found = outcomes(s, statements, names)
        if found != expected[read]:
            failures.append(f"{read}: {short(found)} on the standby, {short(expected[read])} on the primary")
    return failures


def read_failures(expected):
    """The reads that did not succeed on the primary: all must, but the
    stranger's, which must be refused there too (42501)."""
    failures = []
    for read, found in expected.items():
        errors = [outcome for outcome in found if isinstance(outcome, tuple)]
        wanted = [("42501",)] if read == "as a stranger" else []
        if [error[:1] for error in errors] != wanted:
            failures.append(f"{read} on the primary: {short(found)}")
    return failures


def server_state(s):
    """The `SERVER_STATE` reads that failed in session `s`."""
    failures = []
    for sql in SERVER_STATE:
        done = s.run(sql)
        if not done.ok:
            failures.append(f"{sql}: {done.sqlstate} {done.message}")
    return failures


def refusals(s, names):
    """The writes not refused with `REFUSAL` in session `s`."""
    failures = []
    for write, statement in WRITES.items():
        done = s.run(statement.format(**names))
        if (done.sqlstate, done.message.split("\n")[0]) != REFUSAL:
            failures.append(f"{write}: {done.sqlstate} {done.message or done.rows} instead of {REFUSAL}")
    return failures


def short(value, limit=300):
    text = repr(value)
    return text if len(text) <= limit else text[:limit] + "…"


def flush_lsn(s):
    """The primary's WAL flush location: covers every commit made so far
    (`SETTINGS`)."""
    return s.run("SELECT pg_current_wal_flush_lsn()").value()


def wait_for_replay(s, lsn, seconds=REPLAY_SECONDS):
    """Waits until the standby session `s` has replayed WAL up to `lsn`."""
    deadline = time.monotonic() + seconds
    while s.run(f"SELECT pg_last_wal_replay_lsn() >= '{lsn}'::pg_lsn").value() != "t":
        if time.monotonic() > deadline:
            raise RuntimeError(f"the standby did not replay up to {lsn} in {seconds} s")
        time.sleep(0.05)


def shared_cache_works(standby, database):
    """On standby `standby`: one backend loads social's current version, a
    second reads it (from the shared cache, where the cluster shares
    versions), both are listed by `warren.backends()`, `warren.progress()`
    and `warren.cache()` answer, and after `evict` in each nothing is
    pinned."""
    failures = []
    with Session(standby, database) as loader, Session(standby, database) as reader:
        for sql in ("SELECT warren.prewarm('social')", "SELECT count(*) FROM warren.cache()"):
            done = loader.run(sql)
            if not done.ok:
                failures.append(f"{sql} on the standby: {done.sqlstate} {done.message}")
        read = reader.run("SELECT count(*) FROM warren.reachable_set('social', ARRAY[1::bigint], 3)")
        if not read.ok:
            failures.append(f"a read after another backend's prewarm: {read.sqlstate} {read.message}")
        current = loader.run("SELECT warren.info('social') ->> 'current_version'").value()
        listed = loader.run(f"SELECT count(*) FROM warren.backends() WHERE pid IN ({loader.pid}, {reader.pid})")
        progress = loader.run("SELECT count(*) FROM warren.progress()")
        if not progress.ok:
            failures.append(f"warren.progress() on the standby: {progress.sqlstate} {progress.message}")
        if loader.run(shared_cache.SHARES).value() == "t":
            if listed.value() != "2":
                failures.append(f"warren.backends() lists {listed.value()} of the standby's 2 backends")
            shared = loader.run(f"SELECT count(*) FROM warren.shared_cache() "
                                f"WHERE kind = 'version' AND version = {current}").value()
            hits = loader.run(f"SELECT shared_hits FROM warren.backends() WHERE pid = {reader.pid}").value()
            if shared != "1" or int(hits) < 1:
                failures.append(f"the standby's shared cache lists the current version {shared} times "
                                f"and the second backend's shared hits are {hits}")
        elif not listed.ok:
            failures.append(f"warren.backends() on the standby: {listed.sqlstate} {listed.message}")
        for s in (loader, reader):
            done = s.run("SELECT warren.evict('social')")
            if not done.ok:
                failures.append(f"evict on the standby: {done.sqlstate} {done.message}")
        pinned = loader.run("SELECT count(*) FROM warren.shared_cache() WHERE pins <> 0").value()
        if pinned != "0":
            failures.append(f"after evict, {pinned} shared entries are pinned on the standby")
    return failures


CHURN_READ = ("SELECT count(*), sum(n) FROM warren.reachable_set('churn', ARRAY[1::bigint], 100, "
              "version := {version}) n")


class Retirement:
    """Graph `churn` keeps one version. Two standby sessions read its
    current version: one in a REPEATABLE READ transaction left open, one in
    a statement still running. Meanwhile the primary refreshes it (retention
    deletes the version they read) and vacuums the catalog, whose replayed
    cleanup conflicts with their snapshots unless the standby sends
    `hot_standby_feedback` (`feedback`). Each must then read the old
    version consistently or be cancelled by a recovery conflict (40001; the
    idle transaction's connection may be ended instead); a new statement,
    once the standby has replayed it all, must not find the old version,
    and must read the new one as the primary does."""

    def __init__(self, primary, standby, database, feedback):
        self.primary, self.standby, self.database, self.feedback = primary, standby, database, feedback
        self.seen = []  # how each reader ended, for the report

    def run(self):
        with Session(self.primary, self.database) as writer, Session(self.standby, self.database) as watcher:
            if self.feedback:
                send_feedback(writer, watcher)
            old = watcher.run("SELECT warren.info('churn') ->> 'current_version'").value()
            expected = writer.run(CHURN_READ.format(version=old)).rows
            with Session(self.standby, self.database) as holder, \
                    Session(self.standby, self.database) as sleeper:
                checked(holder, "BEGIN ISOLATION LEVEL REPEATABLE READ")
                failures = self.consistent("the open transaction's first read", holder.run(CHURN_READ.format(
                    version=old)), expected)
                sleeping = Statement(sleeper, f"SELECT n.* FROM pg_sleep(4), ({CHURN_READ.format(version=old)}) n")
                wait_until_sleeping(watcher, sleeper.pid)
                done = writer.run("DELETE FROM public.churn_edges WHERE src = 50; SELECT warren.refresh('churn')")
                if not done.ok:
                    return [f"refreshing churn on the primary: {done.sqlstate} {done.message}"]
                checked(writer, "VACUUM warren.segments, warren.versions, warren.vectors, warren.graphs")
                wait_for_replay(watcher, flush_lsn(writer))
                failures += self.reread("the open transaction", holder, old, expected)
                failures += self.finish(sleeping, expected)
            gone = watcher.run(CHURN_READ.format(version=old))
            if (gone.sqlstate, gone.message) != ("42704", f'graph "churn" has no version {old}'):
                failures.append(f"a new statement read the retired version {old}: {gone.sqlstate} "
                                f"{gone.message or gone.rows}")
            new = watcher.run("SELECT warren.info('churn') ->> 'current_version'").value()
            if watcher.run(CHURN_READ.format(version=new)).rows != writer.run(CHURN_READ.format(version=new)).rows:
                failures.append("the standby's current churn version answers differently from the primary's")
        return failures

    def consistent(self, what, done, expected):
        if done.ok and done.rows == expected:
            self.seen.append(f"{what}: consistent")
            return []
        if done.sqlstate == "40001":
            self.seen.append(f"{what}: cancelled (40001)")
            return []
        return [f"{what}: {done.sqlstate} {done.message or done.rows}, expected {expected} or 40001"]

    def reread(self, what, s, old, expected):
        """The open transaction's reads after the retirement: through its
        cache, and after `evict` from the segments again; once one is
        cancelled, the rest fail as an aborted transaction does."""
        failures, cancelled = [], False
        try:
            for step in (CHURN_READ.format(version=old), "SELECT warren.evict('churn')",
                         CHURN_READ.format(version=old)):
                done = s.run(step)
                if cancelled:
                    if done.sqlstate != "25P02":
                        failures.append(f"{what} after its cancellation: {done.sqlstate} {done.message}")
                    continue
                if step.startswith("SELECT warren.evict"):
                    cancelled = done.sqlstate == "40001"
                    failures += [] if done.ok or cancelled else [f"{what}'s evict: {done.sqlstate} {done.message}"]
                    continue
                failures += self.consistent(f"{what}'s read", done, expected)
                cancelled = done.sqlstate == "40001"
            s.run("ROLLBACK")
        except BackendLost:
            self.seen.append(f"{what}: its connection was ended by the conflict")
        return failures

    def finish(self, statement, expected):
        done = statement.outcome()
        if isinstance(done, BackendLost):
            self.seen.append("the running statement: its connection was ended by the conflict")
            return []
        return self.consistent("the running statement", done, expected)


def send_feedback(primary, standby, seconds=REPLAY_SECONDS):
    """Turns `hot_standby_feedback` on in the standby of session `standby`
    and waits until its walsender on the primary holds back cleanup."""
    for sql in ("ALTER SYSTEM SET hot_standby_feedback = on", "SELECT pg_reload_conf()"):
        done = standby.run(sql)
        if not done.ok:
            raise RuntimeError(f"{sql} on the standby: {done.sqlstate} {done.message}")
    deadline = time.monotonic() + seconds
    while primary.run("SELECT count(*) FROM pg_stat_replication WHERE backend_xmin IS NOT NULL").value() == "0":
        if time.monotonic() > deadline:
            raise RuntimeError(f"the standby's feedback did not reach the primary in {seconds} s")
        time.sleep(0.05)


def checked(s, sql):
    """Runs setup statement `sql` in session `s`, raising if it fails."""
    done = s.run(sql)
    if not done.ok:
        raise RuntimeError(f"{sql}: {done.sqlstate} {done.message}")


def wait_until_sleeping(s, pid, seconds=REPLAY_SECONDS):
    """Waits until backend `pid` is in its `pg_sleep`, so its statement has
    taken its snapshot before anything retires what it reads."""
    sql = f"SELECT wait_event = 'PgSleep' FROM pg_stat_activity WHERE pid = {pid}"
    deadline = time.monotonic() + seconds
    while s.run(sql).value() != "t":
        if time.monotonic() > deadline:
            raise RuntimeError(f"backend {pid} did not reach its pg_sleep in {seconds} s")
        time.sleep(0.05)


class Statement:
    """`sql` running in session `s` on a daemon thread (one left behind
    never holds the run open)."""

    def __init__(self, s, sql):
        self.result = None
        self.thread = threading.Thread(target=self._run, args=(s, sql), daemon=True)
        self.thread.start()

    def _run(self, s, sql):
        try:
            self.result = s.run(sql)
        except BackendLost as lost:
            self.result = lost

    def outcome(self, seconds=REPLAY_SECONDS):
        """The statement's outcome, or `BackendLost`, once it ends."""
        self.thread.join(seconds)
        if self.thread.is_alive():
            raise RuntimeError(f"a standby statement did not end in {seconds} s")
        return self.result


def log_problems(text):
    return [f"server log: {line}" for line in server_log.problems(text, ALLOWED)]
