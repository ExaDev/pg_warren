-- pg_warren 0.2.0 -> 0.3.0 (DESIGN.md §13; CONTRIBUTING.md "Releases and
-- upgrades"). Hand-maintained: every schema change made after 0.2.0 is
-- mirrored here in the same commit, and test/upgrade.py checks that
-- ALTER EXTENSION pg_warren UPDATE from 0.2.0 yields the catalog of a fresh
-- 0.3.0 install.
\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.3.0'" to load this file. \quit

-- M7: a graph of several relations may leave its merged CSR unstored, and
-- all-relations reads then merge the relations' rows on the fly (DESIGN.md
-- §5.3). The default keeps it, so graphs created before the update are
-- backfilled true and keep their merged CSR.
ALTER TABLE warren.graphs ADD COLUMN store_merged boolean NOT NULL DEFAULT true;

DROP FUNCTION warren.create_graph(text, text, text, boolean, boolean, boolean, boolean, integer);
CREATE FUNCTION warren."create_graph"(
	"name" TEXT,
	"node_query" TEXT,
	"edge_query" TEXT,
	"directed" bool DEFAULT true,
	"weighted" bool DEFAULT false,
	"has_relations" bool DEFAULT false,
	"build_reverse" bool DEFAULT true,
	"retain_versions" INT DEFAULT 2,
	"store_merged" bool DEFAULT true
) RETURNS void
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'create_graph_wrapper';
