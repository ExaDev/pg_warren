"""The dump/restore check (DESIGN.md §5.1, §12): a database with graphs
(several promoted versions, a compaction, a staged version, persisted
vectors of two roles, a default and a per-role quota on readers' vectors) is dumped with pg_dump in plain and custom formats,
each dump is restored into a fresh database (psql, and pg_restore without
--single-transaction), and the restored pg_warren must answer as the
original did, then accept a new build.

Both databases have pg_warren's optional event trigger on DROP OWNED
(DESIGN.md §5.7), which a superuser creates. The superuser's restore
recreates it.

The trusted variant (DESIGN.md §7): a non-superuser that owns the database
installed pg_warren, so it may not read the catalog. A plain pg_dump fails
for it; with --exclude-table-data='warren.*' it dumps cleanly, without
graphs, which create_graph and refresh then build again. Its restore fails
only at the event trigger, which only a superuser may create."""

import difflib
import re
import subprocess

from .installer import TRUSTED_ROLE, Installer

# pg_warren's optional event trigger, as docs/semantics.md has a superuser
# create it.
TRIGGER = """
    CREATE EVENT TRIGGER warren_drop_owned ON ddl_command_start
        WHEN TAG IN ('DROP OWNED') EXECUTE FUNCTION warren.on_drop_owned();
"""

# Roles are cluster-wide; `check` creates them once.
ROLES = ("dump_owner", "dump_reader")

SEED = """
    CREATE TABLE public.nodes AS SELECT generate_series(1, 8)::bigint AS id;
    CREATE TABLE public.edges AS
        SELECT id AS src, id % 8 + 1 AS dst, CASE WHEN id % 2 = 0 THEN 'a' ELSE 'b' END AS rel,
               id::float8 AS w
        FROM public.nodes;
    CREATE TABLE public.small_nodes AS SELECT generate_series(1, 3) AS id;
    CREATE TABLE public.small_edges AS SELECT 1 AS src, 2 AS dst;
    GRANT SELECT ON public.nodes, public.edges, public.small_nodes, public.small_edges TO dump_owner;
    SET ROLE dump_owner;
    SELECT warren.create_graph('g', 'SELECT id FROM public.nodes',
                               'SELECT src, dst, rel, w FROM public.edges',
                               weighted := true, has_relations := true, retain_versions := 3);
    SELECT warren.create_graph('small', 'SELECT id FROM public.small_nodes',
                               'SELECT src, dst FROM public.small_edges', directed := false);
    SELECT warren.refresh('g');
    SELECT warren.vector_const('g', 'one', 1);
    SELECT warren.refresh('small');
    SELECT warren.grant('g', 'dump_reader');
    SELECT warren.set_vector_quota('g', 1000000);
    SELECT warren.set_vector_quota('g', 2000000, 'dump_reader');
    RESET ROLE;
    DELETE FROM public.nodes WHERE id = 8;
    SELECT warren.refresh('g');
    SELECT warren.compact('g');
    SELECT warren.vector_degree('g', 'deg');
    SET ROLE dump_reader;
    SELECT warren.vector_dense_id('g', 'mine');
    RESET ROLE;
    SELECT warren.vector_from_query('g', 'community', 'SELECT id, id % 3 FROM public.nodes');
    SELECT warren.contract('g', 'community', 'q');
    SELECT warren.project('g', 'deg', 'q', 'members', 'count');
    -- Each node's community size, lifted through the stored membership.
    CREATE FUNCTION public.lifted() RETURNS TABLE (node_id bigint, size float8) LANGUAGE plpgsql AS $$
    BEGIN
      PERFORM warren.lift('q', 'members', 'g', 'sizes', persist := false);
      RETURN QUERY SELECT v.node_id, v.value FROM warren.vector_values('g', 'sizes', NULL::bigint) v;
    END $$;
    INSERT INTO public.nodes VALUES (8);
    SELECT warren.refresh('g', promote := false, label := 'staged');
""" + TRIGGER

# What pg_warren answers, as sorted text rows. Staleness (now() - built_at)
# changes between the two reads, so it is left out.
STATE = {
    "extension": "SELECT extversion FROM pg_extension WHERE extname = 'pg_warren'",
    "info": """
        SELECT g.name, i - 'staleness' - 'versions',
               (SELECT jsonb_agg(v - 'staleness' ORDER BY v ->> 'seq')
                FROM jsonb_array_elements(i -> 'versions') v)
        FROM warren.graphs g, warren.info(g.name) i ORDER BY 1""",
    "bfs": """
        SELECT v.version_id, b.node_id, b.depth, b.via
        FROM warren.versions v JOIN warren.graphs g USING (graph_id),
             warren.bfs(g.name, ARRAY[1::bigint], 8, version := v.version_id) b
        WHERE g.name = 'g' ORDER BY 1, 2""",
    "neighbours": """
        SELECT v.version_id, n
        FROM warren.versions v JOIN warren.graphs g USING (graph_id),
             warren.neighbours(g.name, 1, 'both', version := v.version_id) n
        WHERE g.name = 'small' ORDER BY 1, 2""",
    "lifted": "SELECT * FROM public.lifted() ORDER BY 1",
    "quotas": """
        SELECT g.name, u.role::text, u.vectors, u.bytes, coalesce(u.quota::text, '')
        FROM warren.graphs g, warren.vector_usage(g.name) u ORDER BY 1, 2""",
    "event_triggers": """
        SELECT evtname, evtevent, evtfoid::regprocedure::text, array_to_string(evttags, ','), evtenabled
        FROM pg_event_trigger ORDER BY 1""",
    "vectors": """
        SELECT v.version_id, v.name, v.created_by::text, x.node_id, x.value
        FROM warren.vectors v JOIN warren.graphs g USING (graph_id),
             warren.vector_values(g.name, v.name, NULL::bigint, v.version_id) x
        WHERE g.name = 'g' ORDER BY 1, 2, 4""",
}

# After the restore, the sequences must hand out fresh ids: a new build and
# a new vector of the restored graph.
AFTER_RESTORE = """
    SELECT warren.refresh('g');
    SELECT warren.vector_const('g', 'after', 2);
    SET ROLE dump_reader;
    SELECT warren.vector_const('g', 'after_reader', 3);
    RESET ROLE;
"""


def describe(database):
    return {kind: database.query(query) for kind, query in STATE.items()}


def differences(expected, found, label):
    """A unified diff of two state descriptions, empty if they agree."""
    lines = []
    for kind in STATE:
        a = [",".join(row) for row in expected[kind]]
        b = [",".join(row) for row in found[kind]]
        lines += difflib.unified_diff(a, b, f"dumped: {kind}", f"{label}: {kind}", lineterm="")
    return "\n".join(lines)


def dump_command(cluster, database, path, *options, user="postgres"):
    return [cluster.client("pg_dump"), *cluster.connection(database, user), "-f", str(path), *options]


def restore_command(cluster, database, path, custom, user="postgres", stop_on_error=True):
    """psql for a plain dump, pg_restore (one transaction per item) for a
    custom one; both stop at the first error, and psql only if
    `stop_on_error`."""
    if custom:
        return [cluster.client("pg_restore"), *cluster.connection(database, user), "--exit-on-error", str(path)]
    stop = ["-v", "ON_ERROR_STOP=1"] if stop_on_error else []
    return [cluster.client("psql"), "-X", "-q", *stop, *cluster.connection(database, user), "-f", str(path)]


# pg_dump warns about every foreign-key cycle among the tables it dumps,
# including a table that references itself (warren.versions:
# compacted_from, parent_version), and hints at --disable-triggers. A
# self-reference is checked at the end of its table's COPY, so that warning
# is expected and its hint is not to be followed (DESIGN.md §5.1); any
# other warning is a failure.
EXPECTED_WARNING = "warning: there are circular foreign-key constraints on this table: versions"


SERVER_WARNING = re.compile(r"^(?:psql:[^ ]*: )?WARNING:  (.*)$")


def unexpected_warnings(stderr):
    """The warnings in a client's output, each with its detail lines, other
    than EXPECTED_WARNING: pg_dump's and pg_restore's own, and the server's
    WARNINGs that psql prints."""
    warnings = []
    for line in stderr.splitlines():
        message = re.sub(r"^(pg_dump|pg_restore): ", "", line)
        server = SERVER_WARNING.match(line)
        if server:
            warnings.append([f"warning: {server.group(1)}"])
        elif message.startswith("warning: "):
            warnings.append([message])
        elif message.startswith("detail: ") and warnings:
            warnings[-1].append(message.removeprefix("detail: "))
    found = [f"{w[0]} {', '.join(w[1:])}".strip() for w in warnings]
    return [w for w in found if w != EXPECTED_WARNING]


SERVER_ERROR = re.compile(r"^(?:psql:[^ ]*: )?ERROR:  (.*)$")

# What a non-superuser's restore of a dump with the event trigger fails at:
# the trigger's CREATE EVENT TRIGGER, and so its ALTER EVENT TRIGGER ...
# OWNER TO (DESIGN.md §5.7).
TRIGGER_ERRORS = (
    'permission denied to create event trigger "warren_drop_owned"',
    'event trigger "warren_drop_owned" does not exist',
)


def restore_errors(stderr):
    """The server ERRORs psql printed, as (expected, unexpected): those
    about the event trigger a non-superuser cannot create, and the rest."""
    errors = [match.group(1) for match in map(SERVER_ERROR.match, stderr.splitlines()) if match]
    expected = [error for error in errors if error in TRIGGER_ERRORS]
    return expected, [error for error in errors if error not in TRIGGER_ERRORS]


def _run(command, warnings=None):
    """Runs `command`; returns its error output if it failed, else None.
    Appends pg_dump's unexpected warnings to `warnings`, if given."""
    done = subprocess.run(command, capture_output=True, text=True)
    if done.returncode != 0:
        return f"{' '.join(command[:1])} failed: {done.stdout}{done.stderr}"
    if warnings is not None:
        warnings += unexpected_warnings(done.stderr)
    return None


def check(cluster, directory):
    """Checks the superuser round trip in both formats; returns the failures."""
    admin = cluster.database()
    for role in ROLES:
        admin.execute(f"CREATE ROLE {role}")
    admin.execute("CREATE DATABASE dump_source")
    source = cluster.database("dump_source")
    source.execute("CREATE EXTENSION pg_warren")
    source.execute(SEED)
    expected = describe(source)
    failures = [f"the seeded database has no {kind} to compare" for kind, rows in expected.items() if not rows]
    for label, custom in (("plain", False), ("custom", True)):
        path = directory / f"dump.{label}"
        warnings = []
        error = _run(dump_command(cluster, "dump_source", path, "-Fc" if custom else "-Fp"), warnings)
        if error:
            failures.append(f"{label}: {error}")
            continue
        failures += [f"{label}: pg_dump: {warning}" for warning in warnings]
        warnings = []
        name = f"dump_restored_{label}"
        admin.execute(f"CREATE DATABASE {name}")
        error = _run(restore_command(cluster, name, path, custom), warnings)
        if error:
            failures.append(f"{label} restore: {error}")
            continue
        failures += [f"{label} restore: {warning}" for warning in warnings]
        restored = cluster.database(name)
        diff = differences(expected, describe(restored), f"restored from {label}")
        if diff:
            failures.append(f"{label}: the restored database answers differently\n{diff}")
        error = _run(restored.command("-c", AFTER_RESTORE))
        if error:
            failures.append(f"{label}: a new build after the restore failed: {error}")
    return failures


# The installer's own graph, over its own tables; after a restore without
# the catalog's data, TRUSTED_GRAPH builds it again.
TRUSTED_TABLES = """
    CREATE TABLE public.mine_nodes AS SELECT generate_series(1, 4)::bigint AS id;
    CREATE TABLE public.mine_edges AS SELECT id AS src, id % 4 + 1 AS dst FROM public.mine_nodes;
"""
TRUSTED_GRAPH = """
    SELECT warren.create_graph('mine', 'SELECT id FROM public.mine_nodes',
                               'SELECT src, dst FROM public.mine_edges');
    SELECT warren.refresh('mine');
"""
TRUSTED_BFS = "SELECT node_id, depth FROM warren.bfs('mine', ARRAY[1::bigint], 4) ORDER BY 1"


def check_trusted(cluster, directory):
    """Checks the installer's dumps in a trusted install; returns the failures."""
    admin = cluster.database()
    installer = Installer.trusted()
    admin.execute(f'ALTER ROLE "{TRUSTED_ROLE}" LOGIN')
    admin.execute(installer.create_database_sql("dump_trusted"))
    source = cluster.database("dump_trusted", TRUSTED_ROLE)
    source.execute("CREATE EXTENSION pg_warren")
    source.execute(TRUSTED_TABLES + TRUSTED_GRAPH)
    cluster.database("dump_trusted").execute(TRIGGER)
    expected = source.query(TRUSTED_BFS)
    failures = []
    if _run(dump_command(cluster, "dump_trusted", directory / "denied.sql", user=TRUSTED_ROLE)) is None:
        failures.append("trusted: the installer read the catalog with a plain pg_dump")
    path = directory / "trusted.sql"
    warnings = []
    error = _run(dump_command(cluster, "dump_trusted", path, "--exclude-table-data=warren.*", user=TRUSTED_ROLE),
                 warnings)
    if error:
        return failures + [f"trusted dump: {error}"]
    failures += [f"trusted dump: {warning}" for warning in warnings]
    warnings = []
    admin.execute(installer.create_database_sql("dump_trusted_restored"))
    restore = restore_command(cluster, "dump_trusted_restored", path, False, user=TRUSTED_ROLE, stop_on_error=False)
    done = subprocess.run(restore, capture_output=True, text=True)
    if done.returncode != 0:
        return failures + [f"trusted restore failed: {done.stdout}{done.stderr}"]
    failures += [f"trusted restore: {warning}" for warning in unexpected_warnings(done.stderr)]
    refused, errors = restore_errors(done.stderr)
    failures += [f"trusted restore: ERROR: {error}" for error in errors]
    if refused != list(TRIGGER_ERRORS):
        failures.append(f"trusted restore: expected it to fail at the event trigger, not {refused}")
    restored = cluster.database("dump_trusted_restored", TRUSTED_ROLE)
    found = cluster.database("dump_trusted_restored").query(
        "SELECT extowner::regrole::text, (SELECT count(*) FROM warren.graphs) FROM pg_extension"
        " WHERE extname = 'pg_warren'")
    if found != [[TRUSTED_ROLE, "0"]]:
        failures.append(f"trusted: restored extension owner and graph count are {found}")
    restored.execute(TRUSTED_GRAPH)
    rebuilt = restored.query(TRUSTED_BFS)
    if rebuilt != expected:
        failures.append(f"trusted: the rebuilt graph answers {rebuilt}, not {expected}")
    return failures
