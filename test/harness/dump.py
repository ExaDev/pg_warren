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
only at the event trigger, which only a superuser may create.

The managed variant (DESIGN.md §14 item 51): no superuser at all, as on
Amazon RDS. The administrator is a CREATEROLE, CREATEDB non-superuser in a
role that holds pg_read_all_data and pg_write_all_data with the admin option,
as RDS's rds_superuser does from PG 15. It installs pg_warren, seeds the
graphs, dumps them (and so does a role granted only pg_read_all_data),
restores both dumps into databases it creates, and gets the same answers.
A restore into a cluster that lacks a role the catalog names fails, naming
the role. A graph whose row the administrator rewrites to a superuser owner
(pg_write_all_data allows it) is refused to its refresh. Then the
administrator drops a role's graphs and vectors with warren.drop_owned, and
drops the extension."""

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

# Graphs of two roles over tables of the role that seeds them.
GRAPHS = """
    CREATE TABLE public.nodes AS SELECT generate_series(1, 8)::bigint AS id;
    CREATE TABLE public.edges AS
        SELECT id AS src, id % 8 + 1 AS dst, CASE WHEN id % 2 = 0 THEN 'a' ELSE 'b' END AS rel,
               id::float8 AS w
        FROM public.nodes;
    CREATE TABLE public.small_nodes AS SELECT generate_series(1, 3) AS id;
    CREATE TABLE public.small_edges AS SELECT 1 AS src, 2 AS dst;
    CREATE TABLE public.iri_nodes AS SELECT 'https://example.org/' || id AS id FROM public.nodes;
    INSERT INTO public.iri_nodes VALUES (''), ('é');
    CREATE TABLE public.iri_edges AS SELECT id::varchar AS src, 'é' AS dst FROM public.iri_nodes;
    GRANT SELECT ON public.nodes, public.edges, public.small_nodes, public.small_edges,
                    public.iri_nodes, public.iri_edges TO dump_owner;
    SET ROLE dump_owner;
    SELECT warren.create_graph('g', 'SELECT id FROM public.nodes',
                               'SELECT src, dst, rel, w FROM public.edges',
                               weighted := true, has_relations := true, retain_versions := 3);
    SELECT warren.create_graph('small', 'SELECT id FROM public.small_nodes',
                               'SELECT src, dst FROM public.small_edges', directed := false);
    SELECT warren.refresh('g');
    SELECT warren.vector_const('g', 'one', 1);
    SELECT warren.refresh('small');
    SELECT warren.create_graph('iri', 'SELECT id FROM public.iri_nodes',
                               'SELECT src, dst FROM public.iri_edges');
    SELECT warren.refresh('iri');
    SELECT warren.vector_from_query('iri', 'len', 'SELECT id, length(id) FROM public.iri_nodes');
    SELECT warren.grant('g', 'dump_reader');
    SELECT warren.set_vector_quota('g', 1000000);
    SELECT warren.set_vector_quota('g', 2000000, 'dump_reader');
    RESET ROLE;
    DELETE FROM public.nodes WHERE id = 8;
    SELECT warren.refresh('g');
    DELETE FROM public.iri_nodes WHERE id = '';
    SELECT warren.refresh('iri');
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
"""
SEED = GRAPHS + TRIGGER

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
    "text_ids": """
        SELECT v.version_id, m.node_id, m.dense_id, m.live,
               ARRAY(SELECT warren.neighbours(g.name, m.node_id, 'in', version := v.version_id) n ORDER BY n),
               (SELECT x.value FROM warren.vectors w, warren.vector_values(g.name, w.name, NULL::text, w.version_id) x
                WHERE w.graph_id = g.graph_id AND w.version_id = v.version_id AND x.node_id = m.node_id)
        FROM warren.versions v JOIN warren.graphs g USING (graph_id),
             warren.id_map(g.name, NULL::text, v.version_id) m
        WHERE g.name = 'iri' AND m.live ORDER BY 1, 3""",
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


# A text graph over ids that convert to any encoding, read back in the
# restored database: refused there if that database's encoding is not the
# one its segments record (DESIGN.md §5.7, §14 item 42).
ENCODED_SEED = """
    CREATE TABLE public.ascii_nodes AS SELECT 'node-' || n AS id FROM generate_series(1, 4) n;
    CREATE TABLE public.ascii_edges AS SELECT 'node-1' AS src, 'node-2' AS dst;
    SELECT warren.create_graph('ascii', 'SELECT id FROM public.ascii_nodes',
                               'SELECT src, dst FROM public.ascii_edges');
    SELECT warren.refresh('ascii');
"""
ENCODING_REFUSED = """
    DO $$
    DECLARE
        message text;
    BEGIN
        PERFORM warren.degree('ascii', 'node-1'::text);
        RAISE EXCEPTION 'a UTF8 text graph was read in a LATIN1 database';
    EXCEPTION WHEN object_not_in_prerequisite_state THEN
        GET STACKED DIAGNOSTICS message = MESSAGE_TEXT;
        IF message NOT LIKE '%encoding%' THEN
            RAISE EXCEPTION 'refused for another reason: %', message;
        END IF;
    END $$;
"""


def check_cross_encoding(cluster, directory):
    """Checks that a text graph dumped from a UTF8 database and restored
    into a LATIN1 one is refused there (55000), and still read in a UTF8
    one; returns the failures."""
    admin = cluster.database()
    admin.execute("CREATE DATABASE dump_utf8 ENCODING 'UTF8' TEMPLATE template0 LOCALE 'C'")
    source = cluster.database("dump_utf8")
    source.execute("CREATE EXTENSION pg_warren")
    source.execute(ENCODED_SEED)
    path = directory / "encoded.custom"
    error = _run(dump_command(cluster, "dump_utf8", path, "-Fc"))
    if error:
        return [f"cross-encoding dump: {error}"]
    failures = []
    for name, encoding in (("dump_latin1", "LATIN1"), ("dump_utf8_again", "UTF8")):
        admin.execute(f"CREATE DATABASE {name} ENCODING '{encoding}' TEMPLATE template0 LOCALE 'C'")
        error = _run(restore_command(cluster, name, path, True))
        if error:
            failures.append(f"{encoding} restore: {error}")
            continue
        restored = cluster.database(name)
        if encoding == "LATIN1":
            error = _run(restored.command("-c", ENCODING_REFUSED))
            if error:
                failures.append(f"LATIN1: {error}")
        elif (found := restored.query("SELECT warren.degree('ascii', 'node-1'::text)")) != [["1"]]:
            failures.append(f"UTF8: the restored text graph gives degree {found}, not 1")
    return failures


# A managed service's administrator, emulated (DESIGN.md §14 item 51): on
# Amazon RDS from PG 15, rds_superuser holds pg_read_all_data and
# pg_write_all_data WITH ADMIN OPTION, and the master user is a LOGIN,
# CREATEROLE, CREATEDB non-superuser that inherits it. The cluster superuser
# stands in for rdsadmin, which customers never log in as.
MANAGED_ADMIN = "warren_master"
MANAGED_DUMPER = "warren_dumper"
MANAGED_ROLES = f"""
    CREATE ROLE warren_rds_superuser NOLOGIN;
    GRANT pg_read_all_data, pg_write_all_data TO warren_rds_superuser WITH ADMIN OPTION;
    CREATE ROLE {MANAGED_ADMIN} LOGIN NOSUPERUSER CREATEROLE CREATEDB;
    GRANT warren_rds_superuser TO {MANAGED_ADMIN};
"""
# The administrator would have created the roles that own and read graphs;
# `check` created them as the superuser, so it is made a member here.
MANAGED_MEMBERSHIPS = f"GRANT {', '.join(ROLES)} TO {MANAGED_ADMIN}"
# What the administrator grants, with its admin option, to a role that only
# dumps.
MANAGED_DUMPER_SQL = f"CREATE ROLE {MANAGED_DUMPER} LOGIN; GRANT pg_read_all_data TO {MANAGED_DUMPER}"

MISSING_ROLE = re.compile(r'ERROR:  role "([^"]+)" does not exist')


def missing_roles(stderr):
    """The roles a restore reported missing, once each, in order."""
    return list(dict.fromkeys(MISSING_ROLE.findall(stderr)))


# After the round trip, the administrator's lifecycle: a role's graphs and
# vectors dropped as DROP OWNED would, then the extension.
MANAGED_LIFECYCLE = """
    SELECT warren.drop_owned('dump_reader');
    DROP EXTENSION pg_warren;
"""
MANAGED_AFTER_LIFECYCLE = """
    SELECT (SELECT count(*) FROM pg_namespace WHERE nspname = 'warren'),
           (SELECT count(*) FROM pg_extension WHERE extname = 'pg_warren')"""

# What pg_write_all_data allows (DESIGN.md §7, §14 item 51): the
# administrator rewrites a graph's row to make the bootstrap superuser
# (rdsadmin, on RDS) its owner, with a source query of its choosing. Its
# refresh must be refused (42501) before the query runs as that superuser;
# if it ran, the division by zero would fail the block instead.
MANAGED_SUPERUSER_OWNER = """
    UPDATE warren.graphs SET owner = 10::oid::regrole, node_query = 'SELECT 1 / 0'
    WHERE name = 'g';
    DO $$ BEGIN
        PERFORM warren.refresh('g');
        RAISE EXCEPTION 'the administrator refreshed a graph a superuser owns';
    EXCEPTION WHEN insufficient_privilege THEN NULL;
    END $$;
"""


def check_managed(cluster, directory):
    """Checks dump, restore and the extension's lifecycle with no superuser;
    returns the failures."""
    admin = cluster.database()
    admin.execute(MANAGED_ROLES + MANAGED_MEMBERSHIPS)
    master = cluster.database("postgres", MANAGED_ADMIN)
    master.execute(MANAGED_DUMPER_SQL)
    master.execute("CREATE DATABASE dump_managed")
    source = cluster.database("dump_managed", MANAGED_ADMIN)
    source.execute("CREATE EXTENSION pg_warren")
    source.execute(GRAPHS)
    expected = describe(source)
    failures = []
    if (owner := source.query("SELECT extowner::regrole::text FROM pg_extension WHERE extname = 'pg_warren'")) \
            != [[MANAGED_ADMIN]]:
        failures.append(f"managed: the extension's owner is {owner}")
    dumps = (("plain", False, MANAGED_DUMPER), ("custom", True, MANAGED_ADMIN))
    for label, custom, dumper in dumps:
        path = directory / f"managed.{label}"
        warnings = []
        error = _run(dump_command(cluster, "dump_managed", path, "-Fc" if custom else "-Fp", user=dumper), warnings)
        if error:
            failures.append(f"managed {label} dump by {dumper}: {error}")
            continue
        failures += [f"managed {label} dump by {dumper}: {warning}" for warning in warnings]
        name = f"dump_managed_{label}"
        master.execute(f"CREATE DATABASE {name}")
        warnings = []
        error = _run(restore_command(cluster, name, path, custom, user=MANAGED_ADMIN), warnings)
        if error:
            failures.append(f"managed {label} restore: {error}")
            continue
        failures += [f"managed {label} restore: {warning}" for warning in warnings]
        restored = cluster.database(name, MANAGED_ADMIN)
        diff = differences(expected, describe(restored), f"managed restore from {label}")
        if diff:
            failures.append(f"managed {label}: the restored database answers differently\n{diff}")
        error = _run(restored.command("-c", AFTER_RESTORE))
        if error:
            failures.append(f"managed {label}: a new build after the restore failed: {error}")
    return failures + _missing_role(cluster, directory / "managed.custom") + _superuser_owner(cluster) \
        + _lifecycle(cluster)


def _superuser_owner(cluster):
    """The administrator, rewriting a graph's row to a superuser owner,
    still cannot run SQL as that superuser through the graph."""
    restored = cluster.database("dump_managed_plain", MANAGED_ADMIN)
    error = _run(restored.command("-c", MANAGED_SUPERUSER_OWNER))
    return [f"managed: a graph re-owned to a superuser: {error}"] if error else []


def _missing_role(cluster, path):
    """Restores `path` while a role it names is missing (renamed away, as
    if on another cluster); the restore must fail, naming it."""
    admin = cluster.database()
    cluster.database("postgres", MANAGED_ADMIN).execute("CREATE DATABASE dump_managed_missing")
    admin.execute("ALTER ROLE dump_reader RENAME TO dump_reader_elsewhere")
    try:
        done = subprocess.run(restore_command(cluster, "dump_managed_missing", path, True, user=MANAGED_ADMIN),
                              capture_output=True, text=True)
    finally:
        admin.execute("ALTER ROLE dump_reader_elsewhere RENAME TO dump_reader")
    if done.returncode == 0:
        return ["managed: a restore without the role dump_reader succeeded"]
    if missing_roles(done.stderr) != ["dump_reader"]:
        return [f"managed: a restore without dump_reader failed otherwise: {done.stderr}"]
    return []


def _lifecycle(cluster):
    restored = cluster.database("dump_managed_custom", MANAGED_ADMIN)
    error = _run(restored.command("-c", MANAGED_LIFECYCLE))
    if error:
        return [f"managed lifecycle: {error}"]
    if (left := restored.query(MANAGED_AFTER_LIFECYCLE)) != [["0", "0"]]:
        return [f"managed lifecycle: schema warren and the extension remain: {left}"]
    return []
