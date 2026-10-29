-- pg_warren 0.8.0 -> 0.9.0 (DESIGN.md §13; CONTRIBUTING.md "Releases and
-- upgrades"). Hand-maintained: every schema change made after 0.8.0 is
-- mirrored here in the same commit, and test/upgrade.py checks that
-- ALTER EXTENSION pg_warren UPDATE from 0.8.0 yields the catalog of a fresh
-- 0.9.0 install. In a trusted install it runs as the bootstrap superuser
-- (DESIGN.md §7): create only in schema warren, schema-qualify, and use only
-- exact-match pg_catalog operators and functions in what runs here.
\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.9.0'" to load this file. \quit

-- warren.backends() gains its last column, compute_threads, the compute
-- threads a backend's call holds and, in the cluster row, all backends'
-- (DESIGN.md §5.7, §14 item 52). A function's OUT columns change only by
-- dropping it, so it is created again, with the privileges it had.
DO $upgrade$
DECLARE
    acl text := (SELECT p.proacl::text FROM pg_catalog.pg_proc p
                 WHERE p.oid OPERATOR(pg_catalog.=) 'warren.backends()'::pg_catalog.regprocedure::pg_catalog.oid);
    kept record;
BEGIN
    DROP FUNCTION warren.backends();
    CREATE FUNCTION warren."backends"() RETURNS TABLE (
        "kind" text,
        "pid" int,
        "database" oid,
        "role" regrole,
        "private_bytes" bigint,
        "transient_bytes" bigint,
        "mapped_bytes" bigint,
        "lookups" bigint,
        "private_hits" bigint,
        "shared_hits" bigint,
        "waits" bigint,
        "private_loads" bigint,
        "shared_loads" bigint,
        "loaded_bytes" bigint,
        "load_seconds" float8,
        "private_evictions" bigint,
        "shared_evictions" bigint,
        "refusals" jsonb,
        "since" timestamptz,
        "compute_threads" bigint
    )
    VOLATILE PARALLEL SAFE
    LANGUAGE c AS 'MODULE_PATHNAME', 'backends_wrapper';
    -- An explicit ACL: start from no grants, then grant each entry again.
    IF acl IS NOT NULL THEN
        REVOKE ALL ON FUNCTION warren.backends() FROM PUBLIC;
        FOR kept IN
            SELECT a.grantee, a.is_grantable FROM pg_catalog.aclexplode(acl::pg_catalog.aclitem[]) a
        LOOP
            EXECUTE pg_catalog.format('GRANT EXECUTE ON FUNCTION warren.backends() TO %s%s',
                CASE WHEN kept.grantee OPERATOR(pg_catalog.=) 0::pg_catalog.oid THEN 'PUBLIC'
                     ELSE kept.grantee::pg_catalog.regrole::pg_catalog.text END,
                CASE WHEN kept.is_grantable THEN ' WITH GRANT OPTION' ELSE '' END);
        END LOOP;
    END IF;
    DELETE FROM pg_catalog.pg_init_privs ip
    WHERE ip.classoid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass::pg_catalog.oid
      AND ip.objoid OPERATOR(pg_catalog.=) 'warren.backends()'::pg_catalog.regprocedure::pg_catalog.oid;
END $upgrade$;
