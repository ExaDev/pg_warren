-- pg_warren 0.2.0 -> 0.3.0 (DESIGN.md §13; CONTRIBUTING.md "Releases and
-- upgrades"). Hand-maintained: every schema change made after 0.2.0 is
-- mirrored here in the same commit, and test/upgrade.py checks that
-- ALTER EXTENSION pg_warren UPDATE from 0.2.0 yields the catalog of a fresh
-- 0.3.0 install. In a trusted install it runs as the bootstrap superuser
-- (DESIGN.md §7): create only in schema warren, schema-qualify, and use only
-- exact-match pg_catalog operators and functions in what runs here.
\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.3.0'" to load this file. \quit

-- A dump must restore (DESIGN.md §5.1): break the two foreign-key cycles
-- among the catalog tables, which pg_dump cannot order.
ALTER TABLE warren.graphs DROP CONSTRAINT graphs_current_version_fkey;
ALTER TABLE warren.versions DROP CONSTRAINT versions_label_vector_fkey;

-- graphs.current_version is not a foreign key: with versions.graph_id it
-- would make a cycle, and pg_dump cannot order the data of tables in a
-- foreign-key cycle, so a dump would not restore (DESIGN.md §5.1). These
-- triggers check what that key checked on updates and deletes, and more:
-- the current version is one of the graph's own versions, and cannot be
-- deleted while it is current. They are not equivalent to a key: inserts
-- are not checked (a restore only inserts; cache::open checks what it
-- reads), and they lock only the rows their queries lock. (versions.
-- label_vector, for contraction in v1.1, has no foreign key for the same
-- reason.)
CREATE FUNCTION warren.current_version_is_the_graphs() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    -- FOR KEY SHARE, as a foreign key's check locks: a concurrent delete of
    -- the version waits for this transaction, then fails in its trigger.
    IF NEW.current_version IS NOT NULL AND NOT EXISTS (
        SELECT FROM warren.versions v
        WHERE v.version_id = NEW.current_version AND v.graph_id = NEW.graph_id
        FOR KEY SHARE) THEN
        RAISE EXCEPTION USING ERRCODE = 'foreign_key_violation',
            MESSAGE = format('version %s is not a version of graph "%s"', NEW.current_version, NEW.name);
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER current_version_is_the_graphs BEFORE UPDATE OF current_version ON warren.graphs
    FOR EACH ROW EXECUTE FUNCTION warren.current_version_is_the_graphs();

CREATE FUNCTION warren.current_version_is_kept() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    current_id bigint;
BEGIN
    -- FOR SHARE reads the graph's latest row, not the snapshot's: a
    -- promotion committed after this transaction's snapshot fails the
    -- delete (serialization_failure under REPEATABLE READ, 23503 under READ
    -- COMMITTED), and one in progress makes it wait. Deleted with its
    -- graph, the version finds no row.
    SELECT g.current_version INTO current_id FROM warren.graphs g
    WHERE g.graph_id = OLD.graph_id FOR SHARE;
    IF current_id = OLD.version_id THEN
        RAISE EXCEPTION USING ERRCODE = 'foreign_key_violation',
            MESSAGE = format('version %s is the current version of its graph', OLD.version_id);
    END IF;
    RETURN OLD;
END $$;
CREATE TRIGGER current_version_is_kept BEFORE DELETE ON warren.versions
    FOR EACH ROW EXECUTE FUNCTION warren.current_version_is_kept();
-- Trigger functions cannot be called directly; nobody needs EXECUTE on them.
REVOKE EXECUTE ON FUNCTION warren.current_version_is_the_graphs(), warren.current_version_is_kept()
    FROM PUBLIC;

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

-- eigen and distance: tol defaults to 1e-6, about the rounding of the f32
-- vectors they use by default, below which their explicit residual does
-- not fall (DESIGN.md §6.5, §14 item 21). CREATE OR REPLACE keeps each
-- function's grants and comment.
CREATE OR REPLACE FUNCTION warren."eigen"(
	"graph" TEXT, /* Option < & str > */
	"k" INT, /* Option < i32 > */
	"matrix" TEXT DEFAULT 'adjacency', /* Option < & str > */
	"which" TEXT DEFAULT 'largest', /* Option < & str > */
	"rels" TEXT[] DEFAULT NULL, /* :: std :: option :: Option < Vec < Option < String > > > */
	"weighted" bool DEFAULT NULL, /* Option < bool > */
	"symmetrize" bool DEFAULT true, /* Option < bool > */
	"tol" double precision DEFAULT 1e-6, /* Option < f64 > */
	"max_iter" INT DEFAULT NULL, /* Option < i32 > */
	"vectors_prefix" TEXT DEFAULT NULL, /* Option < & str > */
	"version" bigint DEFAULT NULL, /* Option < i64 > */
	"dtype" TEXT DEFAULT 'f32', /* Option < & str > */
	"persist" bool DEFAULT true /* Option < bool > */
) RETURNS TABLE (
	"i" INT,  /* i32 */
	"eigenvalue" double precision,  /* f64 */
	"residual" double precision  /* f64 */
)
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'eigen_wrapper';

CREATE OR REPLACE FUNCTION warren."distance"(
	"graph" TEXT, /* Option < & str > */
	"v1" bigint, /* Option < i64 > */
	"v2" bigint, /* Option < i64 > */
	"method" TEXT, /* Option < & str > */
	"vectors_prefix" TEXT DEFAULT NULL, /* Option < & str > */
	"k" INT DEFAULT NULL, /* Option < i32 > */
	"x" TEXT DEFAULT NULL, /* Option < & str > */
	"y" TEXT DEFAULT NULL, /* Option < & str > */
	"compared_rows" TEXT DEFAULT 'shared', /* Option < & str > */
	"matrix" TEXT DEFAULT NULL, /* Option < & str > */
	"which" TEXT DEFAULT 'largest', /* Option < & str > */
	"rels" TEXT[] DEFAULT NULL, /* :: std :: option :: Option < Vec < Option < String > > > */
	"weighted" bool DEFAULT NULL, /* Option < bool > */
	"symmetrize" bool DEFAULT true, /* Option < bool > */
	"tol" double precision DEFAULT 1e-6, /* Option < f64 > */
	"max_iter" INT DEFAULT NULL, /* Option < i32 > */
	"dtype" TEXT DEFAULT 'f32', /* Option < & str > */
	"graph2" TEXT DEFAULT NULL /* Option < & str > */
) RETURNS jsonb /* JsonB */
STABLE PARALLEL RESTRICTED 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'distance_wrapper';
