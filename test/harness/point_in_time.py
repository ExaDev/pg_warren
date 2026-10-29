"""A base backup restored, and recovered to a point in time (DESIGN.md
§5.1, §13 0.9.0 "Lifecycle"), as a managed service restores a snapshot or
runs point-in-time recovery: graphs, versions and vectors are ordinary
WAL-logged tables, so a restored cluster holds exactly what was committed
at its target, and pg_warren reads it there.

The check (`test/point_in_time.py`) builds the replica check's graphs
(`replicas.setup_sql`) on a primary that archives its WAL (`SETTINGS`),
loads them into the shared cache, takes a base backup and marks a
restore point; then makes each of `CHANGES` (a refresh and the vectors
rebuilt on it; a staged version of a new graph promoted, a vector
written, one dropped), capturing the graphs' state (`graph_state`) and
marking a restore point after each; then makes `LATER`, which no target
but the end of the archive includes. For each target, and for the end of
the archive, it restores the backup into a new cluster, recovers it
(`recovery_settings`) and checks there that the catalog and every read
are as captured at the target, that the shared cache works and that the
graphs can be written."""

from . import server_log

# The primary's settings: commits flushed and full-page writes, as a
# server that is backed up runs (harness.cluster turns both off), and
# each WAL file archived, once, beside the data directory (the archiver
# runs in it).
SETTINGS = ("synchronous_commit = on", "full_page_writes = on", "archive_mode = on",
            "archive_command = 'mkdir -p ../archive && test ! -f ../archive/%f && cp %p ../archive/%f'")

ARCHIVE = "archive"  # in the primary's cluster directory
BACKUP = "backup"  # likewise
BACKED_UP = "backed up"  # the restore point marked right after the base backup

# The changes after the base backup, each a target: a restore point named
# for it is marked once it is made. The first refreshes a graph, compacts
# it and rebuilds its vectors; the second adds a graph, promotes a staged
# version of it and writes and drops vectors.
CHANGES = (
    ("built", (
        "DELETE FROM public.ties WHERE src % 13 = 0",
        "SELECT warren.refresh('social')",
        # Retention deletes the compacted version the reads name; this
        # one replaces it.
        "SELECT warren.compact('social')",
        "SELECT warren.vector_const('social', 'ones', 1)",
        "SELECT warren.pagerank('social', 'rank')",
        "SELECT warren.vector_from_query('social', 'features', "
        "'SELECT id, ARRAY[id::float8, (id % 5)::float8] FROM public.people')",
        "SELECT warren.spmv('social', 'ones', 'spread')",
        "SELECT * FROM warren.eigen('social', 2, 'laplacian', 'smallest', vectors_prefix := 'lap')",
        "SELECT warren.vector_const('social', 'after_backup', 5)",
    )),
    ("staged", (
        "SELECT warren.create_graph('late', 'SELECT generate_series(1, 30)::bigint', "
        "'SELECT g::bigint, (g % 30 + 1)::bigint FROM generate_series(1, 30) g')",
        "SELECT warren.refresh('late', promote := false, label := 'staged after the backup')",
        "SELECT warren.promote('late', (SELECT max(version_id) FROM warren.versions v JOIN warren.graphs g "
        "USING (graph_id) WHERE g.name = 'late'))",
        "SELECT warren.vector_const('late', 'x', 1)",
        "SELECT warren.drop_vector('social', 'after_backup')",
    )),
)
# After the last restore point: only recovery to the end of the archive
# includes it.
LATER = (
    "SELECT warren.drop_graph('churn')",
    "SELECT warren.refresh('kg', promote := false)",
    "SELECT warren.vector_const('social', 'too_late', 1)",
)

# What the server logs may hold, by SQLSTATE: the stranger's reads
# (42501), the reads of graph `churn` once it is dropped (42704), and
# connections ended when a server stops (57P01).
ALLOWED = {"42501", "42704", "57P01"}

PROMOTED = "SELECT NOT pg_is_in_recovery()"
SWITCH = "SELECT pg_walfile_name(pg_switch_wal())"


def recovery_settings(archive, target):
    """A restored cluster's recovery: WAL from `archive`, up to restore
    point `target` and then promoted, or to the archive's end if `target`
    is None."""
    settings = [f"restore_command = 'cp {archive}/%f %p'"]
    if target is not None:
        settings += [f"recovery_target_name = '{target}'", "recovery_target_action = 'promote'"]
    return settings


def archived_sql(walfile):
    """Whether the archiver has archived WAL file `walfile`."""
    return f"SELECT last_archived_wal >= '{walfile}' FROM pg_stat_archiver"


def holds(outcome):
    """Whether a boolean query's outcome (`archived_sql`, `PROMOTED`) is true."""
    return outcome.value() == "t"


def log_problems(text):
    return [f"server log: {line}" for line in server_log.problems(text, ALLOWED)]
