-- pg_warren 0.3.0 -> 0.4.0 (DESIGN.md §13; CONTRIBUTING.md "Releases and
-- upgrades"). Hand-maintained: every schema change made after 0.3.0 is
-- mirrored here in the same commit, and test/upgrade.py checks that
-- ALTER EXTENSION pg_warren UPDATE from 0.3.0 yields the catalog of a fresh
-- 0.4.0 install. In a trusted install it runs as the bootstrap superuser
-- (DESIGN.md §7): create only in schema warren, schema-qualify, and use only
-- exact-match pg_catalog operators and functions in what runs here.
\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.4.0'" to load this file. \quit

-- Node order (DESIGN.md §14 item 22): how a graph's dense ids are ordered
-- at its first build and at compaction. Graphs created before the update
-- are backfilled 'id', the order every version of theirs was built in.
ALTER TABLE warren.graphs ADD COLUMN node_order text NOT NULL DEFAULT 'id'
    CHECK (node_order IN ('id', 'locality'));

-- create_graph and compact gain a node_order parameter, so each is dropped
-- and created again. That would reset their privileges to the default
-- (EXECUTE to PUBLIC), so any privileges set on them are granted again.
DO $upgrade$
DECLARE
    create_graph_acl pg_catalog.aclitem[] := (
        SELECT p.proacl FROM pg_catalog.pg_proc p
        WHERE p.oid = 'warren.create_graph(text, text, text, boolean, boolean, boolean, boolean, integer, boolean)'::pg_catalog.regprocedure);
    compact_acl pg_catalog.aclitem[] := (
        SELECT p.proacl FROM pg_catalog.pg_proc p
        WHERE p.oid = 'warren.compact(text)'::pg_catalog.regprocedure);
    kept record;
BEGIN
    DROP FUNCTION warren.create_graph(text, text, text, boolean, boolean, boolean, boolean, integer, boolean);
    CREATE FUNCTION warren."create_graph"(
        "name" TEXT,
        "node_query" TEXT,
        "edge_query" TEXT,
        "directed" bool DEFAULT true,
        "weighted" bool DEFAULT false,
        "has_relations" bool DEFAULT false,
        "build_reverse" bool DEFAULT true,
        "retain_versions" INT DEFAULT 2,
        "store_merged" bool DEFAULT true,
        "node_order" TEXT DEFAULT 'id'
    ) RETURNS void
    STRICT
    LANGUAGE c
    AS 'MODULE_PATHNAME', 'create_graph_wrapper';

    DROP FUNCTION warren.compact(text);
    CREATE FUNCTION warren."compact"(
        "name" TEXT,
        "node_order" TEXT DEFAULT NULL
    ) RETURNS bigint
    LANGUAGE c
    AS 'MODULE_PATHNAME', 'compact_wrapper';

    -- An explicit ACL: start from no grants, then grant each entry again.
    IF create_graph_acl IS NOT NULL THEN
        REVOKE ALL ON FUNCTION warren.create_graph(text, text, text, boolean, boolean, boolean,
                                                   boolean, integer, boolean, text) FROM PUBLIC;
    END IF;
    IF compact_acl IS NOT NULL THEN
        REVOKE ALL ON FUNCTION warren.compact(text, text) FROM PUBLIC;
    END IF;
    FOR kept IN
        SELECT f.signature, a.grantee, a.is_grantable
        FROM (VALUES
                ('warren.create_graph(text, text, text, boolean, boolean, boolean, boolean, integer, boolean, text)',
                 create_graph_acl),
                ('warren.compact(text, text)', compact_acl)) f (signature, acl),
             pg_catalog.aclexplode(f.acl) a
    LOOP
        EXECUTE pg_catalog.format('GRANT EXECUTE ON FUNCTION %s TO %s%s', kept.signature,
            CASE WHEN kept.grantee OPERATOR(pg_catalog.=) 0::pg_catalog.oid THEN 'PUBLIC'
                 ELSE kept.grantee::pg_catalog.regrole::pg_catalog.text END,
            CASE WHEN kept.is_grantable THEN ' WITH GRANT OPTION' ELSE '' END);
    END LOOP;
END $upgrade$;
