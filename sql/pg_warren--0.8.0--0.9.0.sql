-- pg_warren 0.8.0 -> 0.9.0 (DESIGN.md §13; CONTRIBUTING.md "Releases and
-- upgrades"). Hand-maintained: every schema change made after 0.8.0 is
-- mirrored here in the same commit, and test/upgrade.py checks that
-- ALTER EXTENSION pg_warren UPDATE from 0.8.0 yields the catalog of a fresh
-- 0.9.0 install. In a trusted install it runs as the bootstrap superuser
-- (DESIGN.md §7): create only in schema warren, schema-qualify, and use only
-- exact-match pg_catalog operators and functions in what runs here.
\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.9.0'" to load this file. \quit

-- Graphs from keys (DESIGN.md §9.3, §14 item 52): the node and edge
-- queries a list of tables' primary and foreign keys gives, and a graph
-- created from them. Two new functions; no table changes.
CREATE FUNCTION warren."queries_from_keys"(tables regclass[])
RETURNS TABLE (node_query text, edge_query text, has_relations boolean)
STRICT STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'queries_from_keys_wrapper';

CREATE FUNCTION warren."create_graph_from_keys"(name text, tables regclass[],
                                                directed boolean DEFAULT true,
                                                build_reverse boolean DEFAULT true,
                                                retain_versions int DEFAULT 2,
                                                store_merged boolean DEFAULT true,
                                                node_order text DEFAULT 'id')
RETURNS void STRICT VOLATILE PARALLEL UNSAFE LANGUAGE c
AS 'MODULE_PATHNAME', 'create_graph_from_keys_wrapper';
