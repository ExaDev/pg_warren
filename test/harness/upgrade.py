"""The upgrade-path check (DESIGN.md §13): for every released version of
pg_warren, `CREATE EXTENSION pg_warren VERSION '<released>'` then
`ALTER EXTENSION pg_warren UPDATE` must give the same catalog as a fresh
install of the version in development: the same functions (signatures,
defaults, results, volatility, parallel safety, strictness, security,
C symbols, settings, grants), relations (owners, columns, types,
nullability, defaults, storage, constraints, indexes, persistence, grants),
types, casts, sequences, schema owner and grants, comments, extension owner
and extension configuration. The installer (the superuser, or a
non-superuser for a trusted install) runs CREATE and ALTER EXTENSION."""

import difflib
import re
import shutil
from pathlib import Path

from .extension import REPOSITORY

SQL = REPOSITORY / "sql"

# Each query describes one kind of object of extension pg_warren as sorted,
# comparable text rows.
CATALOG = {
    "functions": """
        SELECT p.oid::regprocedure::text, pg_get_function_arguments(p.oid),
               pg_get_function_result(p.oid), p.prokind, p.provolatile, p.proparallel,
               p.proisstrict, p.prosecdef, p.proleakproof, p.procost, p.prorows,
               l.lanname, p.prosrc, coalesce(p.probin, ''), coalesce(p.proconfig::text, ''),
               coalesce(p.proacl::text, ''), coalesce(obj_description(p.oid, 'pg_proc'), '')
        FROM pg_proc p JOIN pg_language l ON l.oid = p.prolang
        JOIN pg_depend d ON d.classid = 'pg_proc'::regclass AND d.objid = p.oid
        JOIN pg_extension e ON d.refobjid = e.oid AND d.deptype = 'e' AND e.extname = 'pg_warren'
        ORDER BY 1""",
    "relations": """
        SELECT c.oid::regclass::text, c.relkind, c.relpersistence, pg_get_userbyid(c.relowner),
               coalesce(c.relacl::text, ''),
               coalesce(obj_description(c.oid, 'pg_class'), '')
        FROM pg_class c JOIN pg_depend d ON d.classid = 'pg_class'::regclass AND d.objid = c.oid
        JOIN pg_extension e ON d.refobjid = e.oid AND d.deptype = 'e' AND e.extname = 'pg_warren'
        ORDER BY 1""",
    "columns": """
        SELECT a.attrelid::regclass::text,
               row_number() OVER (PARTITION BY a.attrelid ORDER BY a.attnum), a.attname,
               format_type(a.atttypid, a.atttypmod), a.attnotnull, a.attstorage,
               a.attidentity, a.attgenerated, coalesce(pg_get_expr(ad.adbin, ad.adrelid), ''),
               coalesce(a.attacl::text, '')
        FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid
        LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
        WHERE c.relnamespace = 'warren'::regnamespace AND a.attnum > 0 AND NOT a.attisdropped
        ORDER BY 1, 2""",
    "constraints": """
        SELECT conrelid::regclass::text, conname, pg_get_constraintdef(oid)
        FROM pg_constraint WHERE connamespace = 'warren'::regnamespace ORDER BY 1, 2""",
    "indexes": """
        SELECT indexrelid::regclass::text, pg_get_indexdef(indexrelid)
        FROM pg_index i JOIN pg_class c ON c.oid = i.indrelid
        WHERE c.relnamespace = 'warren'::regnamespace ORDER BY 1""",
    "sequences": """
        SELECT seqrelid::regclass::text, format_type(seqtypid, NULL), seqstart, seqincrement,
               seqmax, seqmin, seqcache, seqcycle
        FROM pg_sequence s JOIN pg_class c ON c.oid = s.seqrelid
        WHERE c.relnamespace = 'warren'::regnamespace ORDER BY 1""",
    "types": """
        SELECT t.oid::regtype::text, t.typtype, coalesce(t.typacl::text, '')
        FROM pg_type t JOIN pg_depend d ON d.classid = 'pg_type'::regclass AND d.objid = t.oid
        JOIN pg_extension e ON d.refobjid = e.oid AND d.deptype = 'e' AND e.extname = 'pg_warren'
        ORDER BY 1""",
    "casts": """
        SELECT format_type(c.castsource, NULL), format_type(c.casttarget, NULL),
               coalesce(c.castfunc::regprocedure::text, ''), c.castcontext, c.castmethod
        FROM pg_cast c JOIN pg_depend d ON d.classid = 'pg_cast'::regclass AND d.objid = c.oid
        JOIN pg_extension e ON d.refobjid = e.oid AND d.deptype = 'e' AND e.extname = 'pg_warren'
        ORDER BY 1, 2""",
    "triggers": """
        SELECT t.tgrelid::regclass::text, t.tgname, pg_get_triggerdef(t.oid)
        FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid
        WHERE c.relnamespace = 'warren'::regnamespace AND NOT tgisinternal ORDER BY 1, 2""",
    "init_privs": """
        SELECT i.classoid::regclass::text,
               CASE WHEN i.classoid = 'pg_proc'::regclass THEN i.objoid::regprocedure::text
                    WHEN i.classoid = 'pg_class'::regclass THEN i.objoid::regclass::text
                    WHEN i.classoid = 'pg_namespace'::regclass THEN i.objoid::regnamespace::text
                    ELSE i.objoid::text END,
               i.objsubid, i.privtype, i.initprivs::text
        FROM pg_init_privs i
        WHERE EXISTS (SELECT FROM pg_depend d JOIN pg_extension e ON d.refobjid = e.oid
                      WHERE d.classid = i.classoid AND d.objid = i.objoid AND d.deptype = 'e'
                        AND e.extname = 'pg_warren')
        ORDER BY 1, 2, 3""",
    "schema": """
        SELECT nspname, pg_get_userbyid(nspowner), coalesce(nspacl::text, ''),
               coalesce(obj_description(oid, 'pg_namespace'), '')
        FROM pg_namespace WHERE nspname = 'warren'""",
    "extension": """
        SELECT extversion, extrelocatable, pg_get_userbyid(extowner),
               coalesce((SELECT string_agg(c::regclass::text, ',' ORDER BY c::regclass::text)
                         FROM unnest(extconfig) c), ''),
               coalesce(array_to_string(extcondition, ','), '')
        FROM pg_extension WHERE extname = 'pg_warren'""",
}


def released_versions():
    """The released versions, from sql/released.sha256."""
    lines = (SQL / "released.sha256").read_text().splitlines()
    return [re.search(r"pg_warren--(.+)\.sql$", line).group(1) for line in lines if line.strip()]


def install_released_scripts(extension_directory):
    """Copies the released install scripts next to the installed extension
    (`cargo pgrx install` installs only the current version's script and
    the upgrade scripts)."""
    for version in released_versions():
        shutil.copy(SQL / f"pg_warren--{version}.sql", Path(extension_directory))


def describe(database):
    return {kind: database.query(query) for kind, query in CATALOG.items()}


def differences(expected, found):
    """A unified diff of two catalog descriptions, empty if they agree."""
    lines = []
    for kind in CATALOG:
        a = [",".join(row) for row in expected[kind]]
        b = [",".join(row) for row in found[kind]]
        lines += difflib.unified_diff(a, b, f"fresh install: {kind}", f"upgraded: {kind}", lineterm="")
    return "\n".join(lines)


# State written under the released version, by its own library and SQL
# functions (see `check`; the control file is the version in development's), to exercise the upgrade's backfills and what
# runs on the upgraded catalog: graph 'upgrade', with three promoted
# versions (the first holding vector 'old', all ones) and a staged one; and
# graph 'upgrade_typed', weighted, of three relations (some edges in two),
# built once, whose bfs, components and (where the release has vectors)
# spmv results are recorded in upgrade_expected. Roles are cluster-wide, so
# check creates upgrade_owner once.
SEED_GRAPH = """
    CREATE TABLE upgrade_nodes AS SELECT generate_series(1, 4)::bigint AS id;
    CREATE TABLE upgrade_edges AS SELECT id AS src, id % 4 + 1 AS dst FROM upgrade_nodes;
    CREATE TABLE upgrade_typed AS
        SELECT s.id AS src, d.id AS dst, CASE WHEN (s.id + d.id) % 2 = 0 THEN 'a' ELSE 'b' END AS rel,
               ((s.id * d.id % 5) / 4.0 + 0.25)::float8 AS w
        FROM upgrade_nodes s, upgrade_nodes d WHERE s.id <> d.id
        UNION ALL SELECT s.id, d.id, 'c', 0.1::float8 FROM upgrade_nodes s, upgrade_nodes d WHERE s.id < d.id;
    GRANT SELECT ON upgrade_nodes, upgrade_edges, upgrade_typed TO upgrade_owner;
    SET ROLE upgrade_owner;
    SELECT warren.create_graph('upgrade', 'SELECT id FROM upgrade_nodes',
                               'SELECT src, dst FROM upgrade_edges');
    RESET ROLE;
    SELECT warren.refresh('upgrade');
"""

# Vector 'old' on the current version, written by the graph owner where the
# release has vector functions, and directly into the catalog where it has
# none (0.1.0, whose warren.vectors has no created_by).
SEED_VECTOR = """
    SET ROLE upgrade_owner;
    SELECT warren.vector_const('upgrade', 'old', 1);
    RESET ROLE;
"""
SEED_VECTOR_DIRECTLY = """
    INSERT INTO warren.vectors (graph_id, version_id, name, dtype, length)
    SELECT graph_id, current_version, 'old', 'f64', 4 FROM warren.graphs WHERE name = 'upgrade';
    INSERT INTO warren.vector_chunks
    SELECT vector_id, 0, decode(repeat('000000000000f03f', 4), 'hex') FROM warren.vectors;
"""

SEED_VERSIONS = """
    SELECT warren.refresh('upgrade');
    SELECT warren.refresh('upgrade');
    SELECT warren.refresh('upgrade', promote := false);
"""

SEED_TYPED = """
    SET ROLE upgrade_owner;
    SELECT warren.create_graph('upgrade_typed', 'SELECT id FROM upgrade_nodes',
                               'SELECT src, dst, rel, w FROM upgrade_typed',
                               weighted := true, has_relations := true);
    RESET ROLE;
    SELECT warren.refresh('upgrade_typed');
    CREATE TABLE upgrade_expected (what text PRIMARY KEY, result text);
"""

# The results recorded under the release and compared after the update, on
# the version 'upgrade_typed' had then (`{version}`: NULL before the update,
# that version after it).
RESULTS = {
    "bfs": """SELECT string_agg(format('%s %s %s', node_id, depth, via), ',' ORDER BY n)
              FROM warren.bfs('upgrade_typed', ARRAY[1::bigint], 3, 'both', version := {version})
                   WITH ORDINALITY b(node_id, depth, via, n)""",
    "components": """SELECT string_agg(format('%s %s', node_id, component_id), ',' ORDER BY node_id)
                     FROM warren.components('upgrade_typed', NULL::bigint, {version})""",
}
SPMV = """
    SELECT warren.spmv('upgrade_typed', 'x', '{y}', version := {version});
    INSERT INTO upgrade_results SELECT 'spmv', string_agg(format('%s %s', node_id, value), ',' ORDER BY node_id)
    FROM warren.vector_values('upgrade_typed', '{y}', NULL::bigint, {version})
"""


def has_vectors(database):
    """Whether the installed version has vector functions (0.2.0 on)."""
    found = database.query(
        "SELECT EXISTS (SELECT FROM pg_proc"
        " WHERE proname = 'vector_const' AND pronamespace = 'warren'::regnamespace)"
    )
    return found == [["t"]]


def results(database, version, spmv):
    """Runs RESULTS on 'upgrade_typed' at `version` into table
    upgrade_results, and if `spmv`, an spmv of its vector x into vector
    `y_<version>`."""
    database.execute("CREATE TABLE IF NOT EXISTS upgrade_results (what text, result text);"
                     " TRUNCATE upgrade_results")
    for what, query in RESULTS.items():
        database.execute(f"INSERT INTO upgrade_results SELECT '{what}', ({query.format(version=version)})")
    if spmv:
        database.execute(SPMV.format(y=f"y_{version}", version=version))


def seed(released):
    """Writes the seed state with the released version's own library and
    SQL functions: `check` has installed the release's build."""
    released.execute(SEED_GRAPH)
    released.execute(SEED_VECTOR if has_vectors(released) else SEED_VECTOR_DIRECTLY)
    released.execute(SEED_VERSIONS)
    released.execute(SEED_TYPED)
    spmv = has_vectors(released)
    if spmv:
        released.execute("SELECT warren.vector_from_query('upgrade_typed', 'x',"
                         " 'SELECT id, id::float8 / 3 FROM upgrade_nodes')")
    results(released, "NULL", spmv)
    released.execute("INSERT INTO upgrade_expected SELECT * FROM upgrade_results")


# Versions of graph 'upgrade' as `seq:staged`.
VERSIONS = """
    SELECT string_agg(seq || ':' || staged, ' ' ORDER BY seq)
    FROM warren.versions JOIN warren.graphs USING (graph_id) WHERE name = 'upgrade'
"""

# Each graph's `store_merged`, and its current version's merged segments
# (with reverse CSRs: four if unweighted, six if weighted).
MERGED = """
    SELECT string_agg(name || ':' || store_merged || ':' || (
               SELECT count(DISTINCT s.segment) FROM warren.segments s
               WHERE s.version_id = g.current_version AND s.segment LIKE '%:merged'),
           ' ' ORDER BY name)
    FROM warren.graphs g
"""
EXPECTED_MERGED = "upgrade:true:0 upgrade_new:false:0 upgrade_typed:true:6"


# Each graph's node order (0.4.0): graphs from before it are backfilled 'id'.
NODE_ORDERS = "SELECT string_agg(name || ':' || node_order, ' ' ORDER BY name) FROM warren.graphs"


def after_update(upgraded):
    """Checks the upgraded state, then runs a promotion on it; returns the
    problems found."""
    problems = []
    orders = upgraded.query(NODE_ORDERS)
    if orders != [["upgrade:id upgrade_typed:id"]]:
        problems.append(f"node orders after the update are {orders}")
    seeded = upgraded.query("SELECT created_by::text || ':' || length FROM warren.vectors WHERE name = 'old'")
    if seeded != [["upgrade_owner:4"]]:
        problems.append(f"backfilled vectors are {seeded}")
    values = upgraded.query(
        "SELECT string_agg(value::text, ',') FROM warren.vector_values('upgrade', 'old', NULL::bigint, 1)")
    if values != [["1,1,1,1"]]:
        problems.append(f"the old vector reads back as {values}")
    # The version 'upgrade_typed' had before the update loads, with its
    # merged CSR, and gives the results the release gave.
    built = upgraded.query("SELECT current_version FROM warren.graphs WHERE name = 'upgrade_typed'")[0][0]
    spmv = upgraded.query("SELECT count(*) FROM upgrade_expected WHERE what = 'spmv'") == [["1"]]
    results(upgraded, built, spmv)
    expected = upgraded.query("SELECT what, result FROM upgrade_expected ORDER BY what")
    found = upgraded.query("SELECT what, result FROM upgrade_results ORDER BY what")
    if found != expected:
        problems.append(f"the pre-update multi-relation version gives {found}, not {expected}")
    empty = upgraded.query("SELECT count(*) FROM upgrade_expected WHERE result IS NULL OR result = ''")
    if empty != [["0"]]:
        problems.append(f"the release recorded empty results: {expected}")
    segments = upgraded.query(
        f"SELECT count(DISTINCT segment) FROM warren.segments WHERE version_id = {built}"
        " AND segment LIKE '%:merged'")
    if segments != [["6"]]:
        problems.append(f"the pre-update multi-relation version has {segments} merged segments, not 6")
    before = upgraded.query(VERSIONS)
    if before != [["1:false 2:false 3:false 4:true"]]:
        problems.append(f"versions before the first promotion are {before}")
    # Retention (retain_versions = 2) on the upgraded catalog: the current
    # version and the newest other promoted one are kept, the one with a
    # vector is kept, the staged one is never deleted, the rest go.
    upgraded.execute("SELECT warren.refresh('upgrade')")
    after = upgraded.query(VERSIONS)
    if after != [["1:false 3:false 4:true 5:false"]]:
        problems.append(f"retention after the update kept {after}")
    written = upgraded.query("SELECT warren.vector_const('upgrade', 'new', 2) IS NOT NULL")
    if written != [["t"]]:
        problems.append(f"a new vector could not be written: {written}")
    # Graphs from before the update keep their merged CSR (store_merged is
    # backfilled true, and a refresh after the update still writes it); a
    # graph created after it may leave it unstored.
    upgraded.execute(
        "SELECT warren.refresh('upgrade_typed');"
        " SELECT warren.create_graph('upgrade_new', 'SELECT id FROM upgrade_nodes',"
        " 'SELECT src, dst, rel FROM upgrade_typed', has_relations := true,"
        " store_merged := false);"
        " SELECT warren.refresh('upgrade_new')")
    merged = upgraded.query(MERGED)
    if merged != [[EXPECTED_MERGED]]:
        problems.append(f"store_merged and merged segments after the update are {merged}")
    # A graph from before the update can be compacted in locality order,
    # which becomes its node order, and one created after it can start in it.
    upgraded.execute(
        "SELECT warren.compact('upgrade_typed', node_order := 'locality');"
        " SELECT warren.create_graph('upgrade_local', 'SELECT id FROM upgrade_nodes',"
        " 'SELECT src, dst FROM upgrade_edges', node_order := 'locality');"
        " SELECT warren.refresh('upgrade_local')")
    orders = upgraded.query(NODE_ORDERS)
    expected = "upgrade:id upgrade_local:locality upgrade_new:id upgrade_typed:locality"
    if orders != [[expected]]:
        problems.append(f"node orders after compacting and creating are {orders}")
    return problems


# Checks of the update from release 0.3.0, in a database of their own:
# privileges set on create_graph and compact under it survive the update,
# which drops and creates both again (each gains a node_order parameter),
# in the catalog and in a dump (CHANGELOG 0.4.0); and an operator planted
# in the extension's schema, which the installer of a trusted install can
# write (it owns `public`), never runs during the update, which runs as the
# bootstrap superuser (DESIGN.md §7). Earlier releases' updates did not
# keep privileges.
FROM_RELEASE = "0.3.0"
GRANTS = """
    REVOKE ALL ON FUNCTION warren.create_graph(text, text, text, boolean, boolean, boolean,
                                               boolean, integer, boolean) FROM PUBLIC;
    GRANT EXECUTE ON FUNCTION warren.create_graph(text, text, text, boolean, boolean, boolean,
                                                  boolean, integer, boolean)
        TO upgrade_owner WITH GRANT OPTION;
    REVOKE ALL ON FUNCTION warren.compact(text) FROM PUBLIC;
    GRANT EXECUTE ON FUNCTION warren.compact(text) TO upgrade_owner;
"""
GRANTED = """
    SELECT string_agg(p.proname || ':' || CASE WHEN a.grantee = 0 THEN 'PUBLIC'
                                               ELSE a.grantee::regrole::text END
                      || ':' || a.is_grantable, ' ' ORDER BY p.proname, a.grantee)
    FROM pg_proc p, aclexplode(p.proacl) a
    WHERE p.pronamespace = 'warren'::regnamespace AND p.proname IN ('create_graph', 'compact')
      AND a.grantee <> p.proowner
"""
EXPECTED_GRANTED = "compact:upgrade_owner:false create_graph:upgrade_owner:true"
# What a dump of the updated database must say about the two functions.
DUMPED_GRANTS = [
    "REVOKE ALL ON FUNCTION warren.compact(name text, node_order text) FROM PUBLIC;",
    "GRANT ALL ON FUNCTION warren.compact(name text, node_order text) TO upgrade_owner;",
    "REVOKE ALL ON FUNCTION warren.create_graph(",
    "TO upgrade_owner WITH GRANT OPTION;",
]
EXTENSION_SCHEMA = "SELECT extnamespace::regnamespace::text FROM pg_extension WHERE extname = 'pg_warren'"
# `=` (oid, regprocedure): an exact match for `oid = '…'::regprocedure`,
# which Postgres prefers to pg_catalog's `=` (oid, oid) after a cast. It
# records that it ran, and as whom.
HOSTILE = """
    CREATE TABLE {schema}.hostile_ran (role text);
    GRANT INSERT ON {schema}.hostile_ran TO PUBLIC;
    CREATE FUNCTION {schema}.hostile_eq(oid, regprocedure) RETURNS boolean
    LANGUAGE plpgsql AS $$
    BEGIN
        INSERT INTO {schema}.hostile_ran VALUES (current_user);
        RETURN $1 OPERATOR(pg_catalog.=) $2::oid;
    END $$;
    CREATE OPERATOR {schema}.= (LEFTARG = oid, RIGHTARG = regprocedure,
                                FUNCTION = {schema}.hostile_eq)
"""
REMOVE_HOSTILE = """
    DROP OPERATOR {schema}.= (oid, regprocedure);
    DROP FUNCTION {schema}.hostile_eq(oid, regprocedure);
    DROP TABLE {schema}.hostile_ran
"""


def update_from_release(cluster, installer, expected):
    """The problems of the update from FROM_RELEASE, installed and updated
    by `installer`, with privileges set on create_graph and compact and an
    operator planted in the extension's schema by the installer; `expected`
    describes a fresh install. The release's install script is in place,
    and the library is the version in development's, whose C symbols the
    release's functions name."""
    name = "upgrade_from_release_checks"
    cluster.database("postgres").execute(installer.create_database_sql(name))
    database = cluster.database(name)
    database.execute(installer.as_installer(f"CREATE EXTENSION pg_warren VERSION '{FROM_RELEASE}'"))
    database.execute(GRANTS)
    schema = database.query(EXTENSION_SCHEMA)[0][0]
    database.execute(installer.as_installer(HOSTILE.format(schema=schema)))
    database.execute(installer.as_installer("ALTER EXTENSION pg_warren UPDATE"))
    problems = []
    ran = database.query(f"SELECT coalesce(string_agg(role, ' '), '') FROM {schema}.hostile_ran")
    if ran != [[""]]:
        problems.append(f"an operator planted in schema {schema} ran during the update as {ran}")
    database.execute(installer.as_installer(REMOVE_HOSTILE.format(schema=schema)))
    granted = database.query(GRANTED)
    if granted != [[EXPECTED_GRANTED]]:
        problems.append(f"privileges after the update are {granted}, not {EXPECTED_GRANTED}")
    init_privs = database.query(CATALOG["init_privs"])
    if init_privs != expected["init_privs"]:
        problems.append(f"initial privileges after the update are {init_privs}, "
                        f"not a fresh install's {expected['init_privs']}")
    dumped = cluster.dump_schema(name)
    missing = [line for line in DUMPED_GRANTS if line not in dumped]
    if missing:
        problems.append(f"a dump after the update lacks {missing}")
    return [f"{FROM_RELEASE} -> update checks: {problem}" for problem in problems]


def library_problem(database, expected, when):
    """None if the pg_warren library a new backend of `database` loads is
    version `expected`, else what is wrong."""
    found = database.query("SELECT warren.version()")[0][0]
    return None if found == expected else f"{when} the library loaded is {found}, not {expected}"


def check(cluster, current, installer, releases, installed):
    """Checks every released version's upgrade path, installed and updated
    by `installer`; returns the failures. For each release, its own build
    (`releases`: library and scripts; the control file stays the version in
    development's, since releases before 0.3.0 were not trusted) is
    installed while its state is seeded, then the version in development
    (`installed`) is put back before the update, which runs in fresh
    backends."""
    installer.prepare(cluster)
    fresh = cluster.database("postgres")
    fresh.execute("CREATE ROLE upgrade_owner")
    fresh.execute(installer.create_database_sql("upgrade_fresh"))
    expected_db = cluster.database("upgrade_fresh")
    expected_db.execute(installer.as_installer("CREATE EXTENSION pg_warren"))
    expected = describe(expected_db)
    failures = []
    for version in released_versions():
        name = f"upgrade_from_{version.replace('.', '_')}"
        fresh.execute(installer.create_database_sql(name))
        upgraded = cluster.database(name)
        releases.install(version)
        installed.restore_control()
        install_released_scripts(releases.extension_directory)
        upgraded.execute(installer.as_installer(f"CREATE EXTENSION pg_warren VERSION '{version}'"))
        problem = library_problem(upgraded, version, f"after installing release {version}")
        if problem:
            failures.append(problem)
            continue
        # In a trusted install the release's own library needs the catalog
        # granted while it seeds; the catalog comparison below checks that
        # revoking it left the privileges as a fresh install has them.
        grants = installer.seeding_grants_sql(True)
        if grants:
            upgraded.execute(grants)
        seed(upgraded)
        if grants:
            upgraded.execute(installer.seeding_grants_sql(False))
        installed.restore()
        problem = library_problem(upgraded, current, f"after restoring {current} over release {version}")
        if problem:
            failures.append(problem)
            continue
        upgraded.execute(installer.as_installer("ALTER EXTENSION pg_warren UPDATE"))
        diff = differences(expected, describe(upgraded))
        if diff:
            failures.append(f"{version} -> {current}: the upgraded catalog differs\n{diff}")
        failures += [f"{version} -> {current}: {problem}" for problem in after_update(upgraded)]
    failures += update_from_release(cluster, installer, expected)
    return failures
