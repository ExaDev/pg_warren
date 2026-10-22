-- What the shared cache's multi-backend tests see (DESIGN.md §5.6), for
-- test/run.py's --shared-cache modes:
-- :shared, whether this cluster shares versions: a cap is set, the
--   registry exists (PG 17 and later, or pg_warren is preloaded), and DSM
--   segments are posix files in /dev/shm, which these tests count;
-- :conninfo, for dblink sessions to this database;
-- shared_segments(version), how many DSM segments hold that version, by
--   the prefix pg_warren writes at their start (magic `WRSV`, this
--   database's oid at bytes 8..12 and the version id at bytes 16..24,
--   little-endian); shared_segment_size(version),
--   the size of the one that does.
CREATE EXTENSION IF NOT EXISTS dblink;
SELECT warren.version() IS NOT NULL AS loaded \gset
SELECT coalesce(current_setting('warren.shared_cache_memory', true), '0') <> '0'
       AND (current_setting('server_version_num')::int >= 170000
            OR current_setting('shared_preload_libraries') ~ 'pg_warren')
       AND current_setting('dynamic_shared_memory_type') = 'posix' AS shared \gset
SELECT format('dbname=%s user=%s host=%s port=%s', current_database(), session_user,
              split_part(current_setting('unix_socket_directories'), ',', 1),
              current_setting('port')) AS conninfo \gset

CREATE FUNCTION shared_segment_files(version bigint) RETURNS SETOF text LANGUAGE sql AS $$
    SELECT name
    FROM pg_catalog.pg_ls_dir('/dev/shm') AS f(name),
         LATERAL pg_catalog.pg_read_binary_file('/dev/shm/' || name, 0, 24, true) AS p(prefix)
    WHERE name LIKE 'PostgreSQL.%' AND length(prefix) = 24
      AND substr(prefix, 1, 4) = 'WRSV'::bytea
      AND (SELECT sum(get_byte(prefix, 16 + i)::bigint << (8 * i))
           FROM generate_series(0, 6) AS i) = version
      AND get_byte(prefix, 23) = 0
      AND (SELECT sum(get_byte(prefix, 8 + i)::bigint << (8 * i))
           FROM generate_series(0, 3) AS i)
          = (SELECT oid FROM pg_catalog.pg_database WHERE datname = current_database())::bigint
$$;
CREATE FUNCTION shared_segments(version bigint) RETURNS bigint LANGUAGE sql AS $$
    SELECT count(*) FROM shared_segment_files(version)
$$;
-- The size of the DSM segment that holds a version, or NULL if none does.
CREATE FUNCTION shared_segment_size(version bigint) RETURNS bigint LANGUAGE sql AS $$
    SELECT max((pg_catalog.pg_stat_file('/dev/shm/' || name, true)).size)
    FROM shared_segment_files(version) AS name
$$;
