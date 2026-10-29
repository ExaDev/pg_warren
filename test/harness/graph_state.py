"""What a database's graphs are, captured before and after something
replaces the server under them (`pg_upgrade`, a base backup restored to a
point in time, the preload added or removed; DESIGN.md §5.1, §13 0.9.0
"Lifecycle"), and what differs.

A `State` holds every pg_warren catalog table's rows (as a count and a
digest of the rows' text, so bytea segments and vector chunks compare
byte for byte), the extension's version, and the answers of the replica
check's reads (`replicas.READS`: traversals, vector reads, `diff`,
`distance`, and compute that persists nothing), with the version ids they
name (`replicas.VERSIONS_SQL`). `writes_work` then checks that the new
server writes: a refresh, persisted vectors, a compaction and drops."""

from dataclasses import dataclass

from . import replicas

TABLES_SQL = ("SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace "
              "WHERE n.nspname = 'warren' AND c.relkind = 'r' ORDER BY 1")
EXTENSION_SQL = "SELECT extversion FROM pg_extension WHERE extname = 'pg_warren'"


def digest_sql(table):
    """Table `table`'s rows: their count and a digest of their text, in no
    particular order."""
    return (f"SELECT count(*), md5(coalesce(string_agg(md5(t::text), '' ORDER BY md5(t::text)), '')) "
            f"FROM warren.{table} t")


@dataclass(frozen=True)
class State:
    catalog: dict
    extension: str
    names: dict
    reads: dict


def capture(s):
    """The `State` of session `s`'s database, read as the superuser."""
    tables = [row[0] for row in checked(s, TABLES_SQL).rows]
    catalog = {table: checked(s, digest_sql(table)).rows for table in tables}
    names = replicas.versions(checked(s, replicas.VERSIONS_SQL).rows[0])
    reads = {read: replicas.outcomes(s, statements, names) for read, statements in replicas.READS.items()}
    return State(catalog, checked(s, EXTENSION_SQL).value(), names, reads)


def differences(before, after):
    """What differs from `before` in `after`, part by part."""
    found = []
    for table in sorted(set(before.catalog) | set(after.catalog)):
        if before.catalog.get(table) != after.catalog.get(table):
            found.append(f"warren.{table}: {before.catalog.get(table)} before, {after.catalog.get(table)} after")
    if before.extension != after.extension:
        found.append(f"pg_warren {before.extension} before, {after.extension} after")
    if before.names != after.names:
        found.append(f"the versions read are {before.names} before, {after.names} after")
    for read, outcome in before.reads.items():
        if after.reads.get(read) != outcome:
            found.append(f"{read}: {replicas.short(outcome)} before, {replicas.short(after.reads.get(read))} after")
    return found


# Writes on the new server, each of which must succeed: a refresh and a
# compaction (of `lineage`, which no read takes vectors of, so the reads
# still answer afterwards), persisted vectors (a constant, the library's
# PageRank, a map) read back, a staged version, and dropping a vector and
# a version.
WRITES = (
    "SELECT warren.refresh('lineage')",
    "SELECT warren.compact('lineage')",
    "SELECT warren.vector_const('social', 'written_after', 2)",
    "SELECT warren.pagerank('social', 'rank_after')",
    "SELECT warren.vector_map('social', 'doubled_after', 'rank_after * 2')",
    "SELECT warren.refresh('kg', promote := false, label := 'staged after')",
    "SELECT warren.drop_vector('social', 'written_after')",
    "SELECT warren.drop_version('kg', (SELECT max(version_id) FROM warren.versions v JOIN warren.graphs g "
    "USING (graph_id) WHERE g.name = 'kg' AND v.staged))",
)
# What the writes must leave: the doubled vector sums to twice PageRank's,
# and the written vector is gone.
WRITTEN = ("SELECT round(warren.vector_reduce('social', 'doubled_after', 'sum')::numeric, 9) "
           "= round(2 * warren.vector_reduce('social', 'rank_after', 'sum')::numeric, 9), "
           "(SELECT count(*) FROM warren.vectors WHERE name = 'written_after')")


def writes_work(s):
    """The `WRITES` that failed in session `s`, or what they left wrong."""
    for sql in WRITES:
        done = s.run(sql)
        if not done.ok:
            return [f"{sql}: {done.sqlstate} {done.message}"]
    left = s.run(WRITTEN)
    if left.rows != [["t", "0"]]:
        return [f"after the writes, {WRITTEN} gave {left.sqlstate} {left.message or left.rows}"]
    return []


def checked(s, sql):
    """The outcome of `sql` in session `s`, raising if it failed."""
    done = s.run(sql)
    if not done.ok:
        raise RuntimeError(f"{sql}: {done.sqlstate} {done.message}")
    return done
