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


# State written under the released version, to exercise the upgrade's
# backfills and what runs on the upgraded catalog: a graph with three
# promoted versions (the first holding vector 'old', all ones) and a staged
# one. Roles are cluster-wide, so check creates upgrade_owner once.
SEED_GRAPH = """
    CREATE TABLE upgrade_nodes AS SELECT generate_series(1, 4)::bigint AS id;
    CREATE TABLE upgrade_edges AS SELECT id AS src, id % 4 + 1 AS dst FROM upgrade_nodes;
    GRANT SELECT ON upgrade_nodes, upgrade_edges TO upgrade_owner;
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
    SELECT graph_id, current_version, 'old', 'f64', 4 FROM warren.graphs;
    INSERT INTO warren.vector_chunks
    SELECT vector_id, 0, decode(repeat('000000000000f03f', 4), 'hex') FROM warren.vectors;
"""

SEED_VERSIONS = """
    SELECT warren.refresh('upgrade');
    SELECT warren.refresh('upgrade');
    SELECT warren.refresh('upgrade', promote := false);
"""


def seed(released):
    """Writes the seed state with the released version's own functions."""
    released.execute(SEED_GRAPH)
    has_vectors = released.query(
        "SELECT EXISTS (SELECT FROM pg_proc"
        " WHERE proname = 'vector_const' AND pronamespace = 'warren'::regnamespace)"
    )
    released.execute(SEED_VECTOR if has_vectors == [["t"]] else SEED_VECTOR_DIRECTLY)
    released.execute(SEED_VERSIONS)


# Versions as `id:staged`, by seq.
VERSIONS = """
    SELECT string_agg(version_id || ':' || staged, ' ' ORDER BY seq) FROM warren.versions
"""


def after_update(upgraded):
    """Checks the upgraded state, then runs a promotion on it; returns the
    problems found."""
    problems = []
    seeded = upgraded.query("SELECT created_by::text || ':' || length FROM warren.vectors WHERE name = 'old'")
    if seeded != [["upgrade_owner:4"]]:
        problems.append(f"backfilled vectors are {seeded}")
    values = upgraded.query(
        "SELECT string_agg(value::text, ',') FROM warren.vector_values('upgrade', 'old', NULL::bigint, 1)")
    if values != [["1,1,1,1"]]:
        problems.append(f"the old vector reads back as {values}")
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
    return problems


def check(cluster, current, installer):
    """Checks every released version's upgrade path, installed and updated
    by `installer`; returns the failures."""
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
        upgraded.execute(installer.as_installer(f"CREATE EXTENSION pg_warren VERSION '{version}'"))
        seed(upgraded)
        upgraded.execute(installer.as_installer("ALTER EXTENSION pg_warren UPDATE"))
        diff = differences(expected, describe(upgraded))
        if diff:
            failures.append(f"{version} -> {current}: the upgraded catalog differs\n{diff}")
        failures += [f"{version} -> {current}: {problem}" for problem in after_update(upgraded)]
    return failures
