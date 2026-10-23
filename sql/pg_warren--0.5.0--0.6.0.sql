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

-- Cleanup after a dropped role (DESIGN.md §5.7, §14 items 44 and 45). The
-- event trigger on DROP OWNED that calls warren.on_drop_owned() is a
-- superuser's opt-in: no extension script creates it.
CREATE FUNCTION warren."reassign_owned"("old" regrole, "new" regrole) RETURNS bigint
STRICT VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'reassign_owned_wrapper';

CREATE FUNCTION warren."drop_owned"(role regrole) RETURNS bigint
STRICT VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'drop_owned_wrapper';

CREATE FUNCTION warren."on_drop_owned"() RETURNS event_trigger
VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'on_drop_owned_wrapper';

CREATE FUNCTION warren."orphans"() RETURNS TABLE (
	"kind" TEXT,
	"graph" TEXT,
	"role" oid
)
STRICT VOLATILE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'orphans_wrapper';

CREATE FUNCTION warren."drop_orphans"() RETURNS bigint
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'drop_orphans_wrapper';

-- Quotas on readers' vectors (DESIGN.md §5.7, §14 items 46 and 47): the
-- graph's default, per-role overrides (dumped with the extension, revoked
-- from PUBLIC as every catalog table is), and the index a quota's sum
-- scans. Existing graphs have no quota.
ALTER TABLE warren.graphs ADD COLUMN reader_vector_quota bigint CHECK (reader_vector_quota >= 0);

CREATE INDEX vectors_created_by ON warren.vectors (graph_id, created_by);

CREATE TABLE warren.vector_quotas (
    graph_id  int NOT NULL REFERENCES warren.graphs ON DELETE CASCADE,
    role      regrole NOT NULL,
    bytes     bigint NOT NULL CHECK (bytes >= 0),
    PRIMARY KEY (graph_id, role)
);
REVOKE ALL ON warren.vector_quotas FROM PUBLIC;
SELECT pg_catalog.pg_extension_config_dump('warren.vector_quotas', '');

CREATE FUNCTION warren."set_vector_quota"(name text, bytes bigint, role regrole DEFAULT NULL)
RETURNS void VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'set_vector_quota_wrapper';

CREATE FUNCTION warren."vector_usage"(name text)
RETURNS TABLE (role regrole, vectors bigint, bytes bigint, quota bigint)
STRICT STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'vector_usage_wrapper';
