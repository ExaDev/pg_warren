-- pg_warren 0.5.0 -> 0.6.0 (DESIGN.md §13; CONTRIBUTING.md "Releases and
-- upgrades"). Hand-maintained: every schema change made after 0.5.0 is
-- mirrored here in the same commit, and test/upgrade.py checks that
-- ALTER EXTENSION pg_warren UPDATE from 0.5.0 yields the catalog of a fresh
-- 0.6.0 install. In a trusted install it runs as the bootstrap superuser
-- (DESIGN.md §7): create only in schema warren, schema-qualify, and use only
-- exact-match pg_catalog operators and functions in what runs here.
\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.6.0'" to load this file. \quit

-- warren.set_owner(name, owner) (DESIGN.md §5.7, §14 item 43): gives a graph
-- a new owner, as ALTER MATERIALIZED VIEW ... OWNER TO does.
CREATE FUNCTION warren."set_owner"(name text, owner regrole) RETURNS void
STRICT VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'set_owner_wrapper';
