-- What the shared cache's multi-backend tests see (DESIGN.md §5.6), for
-- test/run.py's --shared-cache modes:
-- :shared, whether this cluster shares versions: a cap is set, the
--   registry exists (PG 17 and later, or pg_warren is preloaded), and DSM
--   segments are posix files in /dev/shm, which these tests count;
-- :conninfo, for dblink sessions to this database;
-- shared_segments(version), how many DSM segments hold that version, by
--   the prefix pg_warren writes at their start (magic `WRSV`, this
--   database's oid at bytes 8..12, the version id at bytes 16..24 and the
--   kind, 1 for a version, at bytes 24..28, little-endian);
--   shared_segment_size(version), the size of the one that does;
--   shared_vector_segments(vector_id), the same for a persisted vector
--   (kind 2);
-- own_evictions(graph), warren.evict(graph) called as a role that does not
--   own the graph, so that it frees what this backend holds and evicts no
--   shared version (DESIGN.md §14 item 34).
CREATE EXTENSION IF NOT EXISTS dblink;
SELECT warren.version() IS NOT NULL AS loaded \gset
SELECT coalesce(current_setting('warren.shared_cache_memory', true), '0') <> '0'
       AND (current_setting('server_version_num')::int >= 170000
            OR current_setting('shared_preload_libraries') ~ 'pg_warren')
       AND current_setting('dynamic_shared_memory_type') = 'posix' AS shared \gset
SELECT format('dbname=%s user=%s host=%s port=%s', current_database(), session_user,
              split_part(current_setting('unix_socket_directories'), ',', 1),
              current_setting('port')) AS conninfo \gset

CREATE FUNCTION shared_kind_files(kind int, id bigint) RETURNS SETOF text LANGUAGE sql AS $$
    SELECT name
    FROM pg_catalog.pg_ls_dir('/dev/shm') AS f(name),
         LATERAL pg_catalog.pg_read_binary_file('/dev/shm/' || name, 0, 28, true) AS p(prefix)
    WHERE name LIKE 'PostgreSQL.%' AND length(prefix) = 28
      AND substr(prefix, 1, 4) = 'WRSV'::bytea
      AND (SELECT sum(get_byte(prefix, 16 + i)::bigint << (8 * i))
           FROM generate_series(0, 6) AS i) = id
      AND get_byte(prefix, 23) = 0
      AND get_byte(prefix, 24) = kind
      AND get_byte(prefix, 25) = 0 AND get_byte(prefix, 26) = 0 AND get_byte(prefix, 27) = 0
      AND (SELECT sum(get_byte(prefix, 8 + i)::bigint << (8 * i))
           FROM generate_series(0, 3) AS i)
          = (SELECT oid FROM pg_catalog.pg_database WHERE datname = current_database())::bigint
$$;
CREATE FUNCTION shared_segment_files(version bigint) RETURNS SETOF text LANGUAGE sql AS $$
    SELECT shared_kind_files(1, version)
$$;
CREATE FUNCTION shared_vector_segments(vector_id bigint) RETURNS bigint LANGUAGE sql AS $$
    SELECT count(*) FROM shared_kind_files(2, vector_id)
$$;
CREATE FUNCTION shared_segments(version bigint) RETURNS bigint LANGUAGE sql AS $$
    SELECT count(*) FROM shared_segment_files(version)
$$;
-- The size of the DSM segment that holds a version, or NULL if none does.
CREATE FUNCTION shared_segment_size(version bigint) RETURNS bigint LANGUAGE sql AS $$
    SELECT max((pg_catalog.pg_stat_file('/dev/shm/' || name, true)).size)
    FROM shared_segment_files(version) AS name
$$;

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
