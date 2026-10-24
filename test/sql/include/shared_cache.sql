-- What the shared cache's multi-backend tests see (DESIGN.md §5.6), for
-- test/run.py's --shared-cache modes:
-- :shared, whether this cluster shares versions: a cap is set and the
--   registry exists (PG 17 and later, or pg_warren is preloaded);
-- :conninfo, for dblink sessions to this database;
-- shared_segments(version), how many DSM segments hold that version: the
--   registry's slots that warren.shared_cache() lists for it, as each
--   slot that is not empty holds one segment, created with the slot and
--   freed once it empties (read from the registry, not from /dev/shm,
--   which only Linux has); shared_segment_size(version), the size of the
--   one that does; shared_vector_segments(vector_id), the same for a
--   persisted vector;
-- unnamed_shared_bytes(), the bytes of the segments the view lists no row
--   for: other databases', earlier installations', and those of versions
--   and vectors whose rows are gone (a test compares it with its value at
--   the start, :unnamed, to see those freed);
-- own_evictions(graph), warren.evict(graph) called as a role that does not
--   own the graph, so that it frees what this backend holds and evicts no
--   shared version (DESIGN.md §14 item 34).
CREATE EXTENSION IF NOT EXISTS dblink;
SELECT warren.version() IS NOT NULL AS loaded \gset
SELECT coalesce(current_setting('warren.shared_cache_memory', true), '0') <> '0'
       AND (current_setting('server_version_num')::int >= 170000
            OR current_setting('shared_preload_libraries') ~ 'pg_warren') AS shared \gset
SELECT format('dbname=%s user=%s host=%s port=%s', current_database(), session_user,
              split_part(current_setting('unix_socket_directories'), ',', 1),
              current_setting('port')) AS conninfo \gset

CREATE FUNCTION shared_segments(version bigint) RETURNS bigint LANGUAGE sql AS $$
    SELECT count(*) FROM warren.shared_cache() AS s WHERE s.kind = 'version' AND s.version = $1
$$;
CREATE FUNCTION shared_vector_segments(vector_id bigint) RETURNS bigint LANGUAGE sql AS $$
    SELECT count(*) FROM warren.shared_cache() AS s WHERE s.kind = 'vector' AND s.vector_id = $1
$$;
-- The size of the DSM segment that holds a version, or NULL if none does.
CREATE FUNCTION shared_segment_size(version bigint) RETURNS bigint LANGUAGE sql AS $$
    SELECT max(s.bytes) FROM warren.shared_cache() AS s WHERE s.kind = 'version' AND s.version = $1
$$;
CREATE FUNCTION unnamed_shared_bytes() RETURNS bigint LANGUAGE sql AS $$
    SELECT sum(CASE WHEN s.kind = 'cluster' THEN s.bytes ELSE -s.bytes END)::bigint FROM warren.shared_cache() AS s
$$;
SELECT unnamed_shared_bytes() AS unnamed \gset

DO $$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = 'warren_stranger') THEN
        CREATE ROLE warren_stranger;
    END IF;
END
$$;
CREATE FUNCTION own_evictions(graph text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    role text := pg_catalog.current_setting('role');
    evicted bigint;
BEGIN
    PERFORM pg_catalog.set_config('role', 'warren_stranger', true);
    evicted := warren.evict(graph);
    PERFORM pg_catalog.set_config('role', role, true);
    RETURN evicted;
END
$$;
