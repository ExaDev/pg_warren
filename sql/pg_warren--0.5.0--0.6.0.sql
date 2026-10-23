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

-- Observability (DESIGN.md §5.7, §14 items 48 and 49): this backend's
-- private cache, and every backend's from the activity table in shared
-- memory. warren.shared_cache() keeps its shape.
CREATE FUNCTION warren."cache"() RETURNS TABLE (
	"kind" TEXT,
	"graph" TEXT,
	"version" bigint,
	"vector" TEXT,
	"vector_id" bigint,
	"bytes" bigint,
	"state" TEXT,
	"last_use" bigint,
	"cap" bigint
)
STRICT VOLATILE PARALLEL RESTRICTED
LANGUAGE c
AS 'MODULE_PATHNAME', 'cache_wrapper';

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
	"since" timestamptz
)
VOLATILE PARALLEL SAFE
LANGUAGE c AS 'MODULE_PATHNAME', 'backends_wrapper';
