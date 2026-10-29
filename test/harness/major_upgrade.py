"""A major-version upgrade with `pg_upgrade` and graphs present (DESIGN.md
§5.1, §13 0.9.0 "Lifecycle"), as a managed service runs one: the old
cluster's catalog, segments and vectors are ordinary tables, so
`pg_upgrade` carries them over as files, and pg_warren, installed in the
new major's tree, reads them there.

The check (`test/major_upgrade.py`) builds the replica check's graphs
(`replicas.setup_sql`) on a cluster of the old major, reads them and loads
them into the shared cache, upgrades the cluster, and checks on the new
one that every catalog table holds the same rows, every read answers as
before (`graph_state`), the shared cache works, `ALTER EXTENSION ...
UPDATE` has nothing to do, and the graphs can be written."""

from . import server_log

# What the server logs may hold, by SQLSTATE: the stranger's reads
# (42501), and connections ended when a server stops (57P01).
ALLOWED = {"42501", "57P01"}

# After the upgrade the extension is at the version it was: the same
# build is installed in both trees.
UPDATE = "ALTER EXTENSION pg_warren UPDATE"


def log_problems(text):
    return [f"server log: {line}" for line in server_log.problems(text, ALLOWED, server_log.CLEAN_SHUTDOWN)]
