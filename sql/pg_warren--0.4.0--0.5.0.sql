-- pg_warren 0.4.0 -> 0.5.0 (DESIGN.md §13; CONTRIBUTING.md "Releases and
-- upgrades"). Hand-maintained: every schema change made after 0.4.0 is
-- mirrored here in the same commit, and test/upgrade.py checks that
-- ALTER EXTENSION pg_warren UPDATE from 0.4.0 yields the catalog of a fresh
-- 0.5.0 install. In a trusted install it runs as the bootstrap superuser
-- (DESIGN.md §7): create only in schema warren, schema-qualify, and use only
-- exact-match pg_catalog operators and functions in what runs here.
\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.5.0'" to load this file. \quit

-- warren.shared_cache() (DESIGN.md §14 item 37): what the shared cache holds
-- of this database's graphs that the caller may read, and the cluster's
-- totals against the cap.
CREATE FUNCTION warren."shared_cache"() RETURNS TABLE (
    "kind" TEXT,
    "graph" TEXT,
    "version" bigint,
    "vector" TEXT,
    "vector_id" bigint,
    "state" TEXT,
    "bytes" bigint,
    "pins" INT,
    "mappers" INT,
    "last_use" bigint,
    "cap" bigint
)
STRICT VOLATILE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'shared_cache_wrapper';
