-- pg_warren 0.7.0 -> 0.8.0 (DESIGN.md §13; CONTRIBUTING.md "Releases and
-- upgrades"). Hand-maintained: every schema change made after 0.7.0 is
-- mirrored here in the same commit, and test/upgrade.py checks that
-- ALTER EXTENSION pg_warren UPDATE from 0.7.0 yields the catalog of a fresh
-- 0.8.0 install. In a trusted install it runs as the bootstrap superuser
-- (DESIGN.md §7): create only in schema warren, schema-qualify, and use only
-- exact-match pg_catalog operators and functions in what runs here.
\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.8.0'" to load this file. \quit

-- warren.pagerank is a native kernel, no longer a PL/pgSQL function over
-- the primitives (DESIGN.md §6.7, §13): the same signature, results,
-- SQLSTATEs and progress. CREATE OR REPLACE keeps the function's grants
-- and anything that depends on it; its SET search_path goes, since the
-- library's access guard pins search_path for the whole call (§7).
CREATE OR REPLACE FUNCTION warren.pagerank(graph text, "out" text, damping float8 DEFAULT 0.85,
                                           tol float8 DEFAULT 1e-6, max_iter int DEFAULT 100,
                                           seed text DEFAULT NULL, rels text[] DEFAULT NULL,
                                           weighted boolean DEFAULT NULL, version bigint DEFAULT NULL,
                                           per_relation boolean DEFAULT false)
RETURNS int
VOLATILE PARALLEL UNSAFE LANGUAGE c AS 'MODULE_PATHNAME', 'pagerank_wrapper';

-- warren.leiden is native (DESIGN.md §6.7): its levels are held in memory,
-- each contracted from the one before, rather than written as level
-- graphs. Its signature is unchanged, so it is replaced in place, keeping
-- any privileges set on it; it no longer sets search_path, which
-- access::entry pins for the call as for every native function.
CREATE OR REPLACE FUNCTION warren.leiden(graph text, "out" text, resolution double precision DEFAULT 1.0,
                              seed bigint DEFAULT 0, max_levels integer DEFAULT 100,
                              tolerance double precision DEFAULT 1e-7,
                              randomness double precision DEFAULT 0.01, iterations integer DEFAULT 2,
                              weighted boolean DEFAULT NULL, levels_prefix text DEFAULT NULL,
                              version bigint DEFAULT NULL, per_relation boolean DEFAULT false)
RETURNS double precision
VOLATILE PARALLEL UNSAFE LANGUAGE c AS 'MODULE_PATHNAME', 'leiden_wrapper';
