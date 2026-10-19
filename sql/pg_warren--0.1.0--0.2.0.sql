-- pg_warren 0.1.0 -> 0.2.0 (DESIGN.md §13; CONTRIBUTING.md "Releases and
-- upgrades"). Hand-maintained: every schema change made after 0.1.0 is
-- mirrored here in the same commit, and test/upgrade.py checks that
-- ALTER EXTENSION pg_warren UPDATE from 0.1.0 yields the catalog of a fresh
-- 0.2.0 install.
\echo Use "ALTER EXTENSION pg_warren UPDATE TO '0.2.0'" to load this file. \quit

-- Vectors record the role that wrote them (§7). Vectors written before
-- 0.2.0 (only possible by direct catalog writes) are the graph owner's.
ALTER TABLE warren.vectors ADD COLUMN created_by regrole;
UPDATE warren.vectors v SET created_by = g.owner
FROM warren.graphs g WHERE g.graph_id = v.graph_id;
ALTER TABLE warren.vectors ALTER COLUMN created_by SET NOT NULL;

-- IEEE floats rarely compress, so skip pglz's attempt on every write.
ALTER TABLE warren.vector_chunks ALTER COLUMN data SET STORAGE EXTERNAL;

-- The vector and compute functions (M4, DESIGN.md §6.2, §6.3), as pgrx
-- generates them for 0.2.0.

CREATE  FUNCTION warren."drop_vector"(
	"graph" TEXT, /* Option < & str > */
	"name" TEXT, /* Option < & str > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS void
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'drop_vector_wrapper';

CREATE  FUNCTION warren."explain_map"(
	"graph" TEXT, /* Option < & str > */
	"expr" TEXT, /* Option < & str > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS jsonb /* JsonB */
STABLE PARALLEL SAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'explain_map_wrapper';

CREATE  FUNCTION warren."spmm"(
	"graph" TEXT, /* Option < & str > */
	"x" TEXT, /* Option < & str > */
	"y" TEXT, /* Option < & str > */
	"semiring" TEXT DEFAULT 'plus_times', /* Option < & str > */
	"transpose" bool DEFAULT false, /* Option < bool > */
	"rels" TEXT[] DEFAULT NULL, /* :: std :: option :: Option < Vec < Option < String > > > */
	"weighted" bool DEFAULT NULL, /* Option < bool > */
	"mask" TEXT DEFAULT NULL, /* Option < & str > */
	"mask_complement" bool DEFAULT false, /* Option < bool > */
	"replace" bool DEFAULT false, /* Option < bool > */
	"accumulate" TEXT DEFAULT NULL, /* Option < & str > */
	"persist" bool DEFAULT true, /* Option < bool > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS void
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'spmm_wrapper';

CREATE  FUNCTION warren."spmv"(
	"graph" TEXT, /* Option < & str > */
	"x" TEXT, /* Option < & str > */
	"y" TEXT, /* Option < & str > */
	"semiring" TEXT DEFAULT 'plus_times', /* Option < & str > */
	"transpose" bool DEFAULT false, /* Option < bool > */
	"rels" TEXT[] DEFAULT NULL, /* :: std :: option :: Option < Vec < Option < String > > > */
	"weighted" bool DEFAULT NULL, /* Option < bool > */
	"mask" TEXT DEFAULT NULL, /* Option < & str > */
	"mask_complement" bool DEFAULT false, /* Option < bool > */
	"replace" bool DEFAULT false, /* Option < bool > */
	"accumulate" TEXT DEFAULT NULL, /* Option < & str > */
	"persist" bool DEFAULT true, /* Option < bool > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS void
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'spmv_wrapper';

CREATE  FUNCTION warren."vector_const"(
	"graph" TEXT, /* Option < & str > */
	"name" TEXT, /* Option < & str > */
	"value" double precision, /* Option < f64 > */
	"dtype" TEXT DEFAULT 'f64', /* Option < & str > */
	"persist" bool DEFAULT true, /* Option < bool > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS bigint /* Option < i64 > */
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'vector_const_wrapper';

CREATE  FUNCTION warren."vector_degree"(
	"graph" TEXT, /* Option < & str > */
	"name" TEXT, /* Option < & str > */
	"direction" TEXT DEFAULT 'out', /* Option < & str > */
	"rels" TEXT[] DEFAULT NULL, /* :: std :: option :: Option < Vec < Option < String > > > */
	"dtype" TEXT DEFAULT 'f64', /* Option < & str > */
	"persist" bool DEFAULT true, /* Option < bool > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS bigint /* Option < i64 > */
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'vector_degree_wrapper';

CREATE  FUNCTION warren."vector_dense_id"(
	"graph" TEXT, /* Option < & str > */
	"name" TEXT, /* Option < & str > */
	"dtype" TEXT DEFAULT 'i64', /* Option < & str > */
	"persist" bool DEFAULT true, /* Option < bool > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS bigint /* Option < i64 > */
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'vector_dense_id_wrapper';

CREATE  FUNCTION warren."vector_from_query"(
	"graph" TEXT, /* Option < & str > */
	"name" TEXT, /* Option < & str > */
	"query" TEXT, /* Option < & str > */
	"dtype" TEXT DEFAULT 'f64', /* Option < & str > */
	"fill" double precision DEFAULT 0, /* Option < f64 > */
	"persist" bool DEFAULT true, /* Option < bool > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS bigint /* Option < i64 > */
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'vector_from_query_wrapper';

CREATE  FUNCTION warren."vector_map"(
	"graph" TEXT, /* Option < & str > */
	"y" TEXT, /* Option < & str > */
	"expr" TEXT, /* Option < & str > */
	"persist" bool DEFAULT true, /* Option < bool > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS void
VOLATILE PARALLEL UNSAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'vector_map_wrapper';

CREATE  FUNCTION warren."vector_norm_diff"(
	"graph" TEXT, /* Option < & str > */
	"a" TEXT, /* Option < & str > */
	"b" TEXT, /* Option < & str > */
	"norm" TEXT DEFAULT 'l2', /* Option < & str > */
	"a_version" bigint DEFAULT NULL, /* Option < i64 > */
	"b_version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS double precision /* f64 */
STABLE PARALLEL SAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'vector_norm_diff_wrapper';

CREATE  FUNCTION warren."vector_reduce"(
	"graph" TEXT, /* Option < & str > */
	"x" TEXT, /* Option < & str > */
	"op" TEXT, /* Option < & str > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS double precision /* Option < f64 > */
STABLE PARALLEL SAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'vector_reduce_wrapper';

CREATE  FUNCTION warren."vector_stats"(
	"graph" TEXT, /* Option < & str > */
	"name" TEXT, /* Option < & str > */
	"version" bigint DEFAULT NULL /* Option < i64 > */
) RETURNS jsonb /* JsonB */
STABLE PARALLEL SAFE 
LANGUAGE c /* Rust */
AS 'MODULE_PATHNAME', 'vector_stats_wrapper';

CREATE FUNCTION warren.id_map(graph text, id_type anyelement, version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, dense_id bigint, live boolean)
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'id_map_wrapper';

CREATE FUNCTION warren.matrix_rows(graph text, name text, id_type anyelement,
                                   version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, "row" float8[])
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'matrix_rows_wrapper';

CREATE FUNCTION warren.vector_topk(graph text, name text, k int, id_type anyelement,
                                   descending bool DEFAULT true, version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, value float8)
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'vector_topk_wrapper';

CREATE FUNCTION warren.vector_values(graph text, name text, id_type anyelement,
                                     version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, value float8)
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'vector_values_wrapper';

-- gather (M5, DESIGN.md §6.4).

CREATE FUNCTION warren.gather(graph text, messages text, id_type anyelement,
                              value_type anycompatible, combine text DEFAULT 'collect',
                              direction text DEFAULT 'in', rels text[] DEFAULT NULL,
                              mask text DEFAULT NULL, mask_complement boolean DEFAULT false,
                              version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, value anycompatible)
VOLATILE PARALLEL UNSAFE LANGUAGE c AS 'MODULE_PATHNAME', 'gather_wrapper';
