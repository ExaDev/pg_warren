//! The catalog tables, created by the extension script (DESIGN.md §5.1):
//! `warren.graphs`, `versions`, `segments`, `vectors` and `vector_chunks`,
//! revoked from `PUBLIC` and registered with `pg_extension_config_dump`.

// Emitted at the end of the script (`finalize`), after `CREATE SCHEMA warren`.
// Functions reach these tables only at run time, through SPI.
pgrx::extension_sql!(
    r#"
CREATE TABLE warren.graphs (
    graph_id         serial PRIMARY KEY,
    name             text UNIQUE NOT NULL,
    owner            regrole NOT NULL,
    readers          regrole[] NOT NULL DEFAULT '{}',
    source           text NOT NULL DEFAULT 'query',
    node_query       text,
    edge_query       text,
    search_path      text NOT NULL,
    id_type          regtype NOT NULL,
    directed         boolean NOT NULL DEFAULT true,
    build_reverse    boolean NOT NULL DEFAULT true,
    weighted         boolean NOT NULL DEFAULT false,
    has_relations    boolean NOT NULL DEFAULT false,
    retain_versions  int NOT NULL DEFAULT 2 CHECK (retain_versions >= 1),
    current_version  bigint
);

CREATE TABLE warren.versions (
    version_id   bigserial PRIMARY KEY,
    graph_id     int NOT NULL REFERENCES warren.graphs ON DELETE CASCADE,
    seq          bigint NOT NULL,
    staged       boolean NOT NULL DEFAULT true,
    built_at     timestamptz NOT NULL,
    built_lsn    pg_lsn NOT NULL,
    node_count   bigint NOT NULL,
    id_space     bigint NOT NULL,
    edge_count   bigint NOT NULL,
    dropped_edges bigint NOT NULL,
    deduplicated_edges bigint NOT NULL,
    target_width  smallint NOT NULL CHECK (target_width IN (4, 8)),
    compacted_from bigint REFERENCES warren.versions ON DELETE SET NULL,
    bytes        bigint NOT NULL,
    label        text,
    parent_version bigint REFERENCES warren.versions,
    label_vector   bigint,
    UNIQUE (graph_id, seq)
);

ALTER TABLE warren.graphs
    ADD FOREIGN KEY (current_version) REFERENCES warren.versions;

CREATE TABLE warren.segments (
    version_id  bigint NOT NULL REFERENCES warren.versions ON DELETE CASCADE,
    segment     text   NOT NULL,
    chunk_no    int    NOT NULL,
    data        bytea  NOT NULL,
    PRIMARY KEY (version_id, segment, chunk_no)
);
ALTER TABLE warren.segments ALTER COLUMN data SET STORAGE EXTERNAL;

CREATE TABLE warren.vectors (
    vector_id   bigserial PRIMARY KEY,
    graph_id    int NOT NULL REFERENCES warren.graphs ON DELETE CASCADE,
    version_id  bigint NOT NULL REFERENCES warren.versions ON DELETE CASCADE,
    name        text NOT NULL,
    dtype       text NOT NULL,
    width       int  NOT NULL DEFAULT 1,
    length      bigint NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    created_by  regrole NOT NULL,
    UNIQUE (graph_id, version_id, name)
);

CREATE TABLE warren.vector_chunks (
    vector_id bigint REFERENCES warren.vectors ON DELETE CASCADE,
    chunk_no  int,
    data      bytea NOT NULL,
    PRIMARY KEY (vector_id, chunk_no)
);
-- IEEE floats rarely compress, so skip pglz's attempt on every write.
ALTER TABLE warren.vector_chunks ALTER COLUMN data SET STORAGE EXTERNAL;

ALTER TABLE warren.versions
    ADD FOREIGN KEY (label_vector) REFERENCES warren.vectors;

-- Only warren.* functions touch these, as the extension owner (§7).
REVOKE ALL ON ALL TABLES IN SCHEMA warren FROM PUBLIC;
REVOKE ALL ON ALL SEQUENCES IN SCHEMA warren FROM PUBLIC;
GRANT USAGE ON SCHEMA warren TO PUBLIC;

SELECT pg_catalog.pg_extension_config_dump('warren.graphs', '');
SELECT pg_catalog.pg_extension_config_dump('warren.graphs_graph_id_seq', '');
SELECT pg_catalog.pg_extension_config_dump('warren.versions', '');
SELECT pg_catalog.pg_extension_config_dump('warren.versions_version_id_seq', '');
SELECT pg_catalog.pg_extension_config_dump('warren.segments', '');
SELECT pg_catalog.pg_extension_config_dump('warren.vectors', '');
SELECT pg_catalog.pg_extension_config_dump('warren.vectors_vector_id_seq', '');
SELECT pg_catalog.pg_extension_config_dump('warren.vector_chunks', '');
"#,
    name = "catalog",
    finalize,
);

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    fn columns(table: &str) -> Result<Option<String>, Error> {
        Spi::get_one::<String>(&format!(
            "SELECT string_agg(attname || ' ' || format_type(atttypid, atttypmod)
                               || CASE WHEN attnotnull THEN ' not null' ELSE '' END,
                               ', ' ORDER BY attnum)
             FROM pg_attribute
             WHERE attrelid = 'warren.{table}'::regclass AND attnum > 0 AND NOT attisdropped"
        ))
    }

    #[pg_test]
    fn graphs_table_matches_the_design() -> Result<(), Error> {
        assert_eq!(
            columns("graphs")?.as_deref(),
            Some(
                "graph_id integer not null, name text not null, owner regrole not null, \
                 readers regrole[] not null, source text not null, node_query text, \
                 edge_query text, search_path text not null, id_type regtype not null, directed boolean not null, \
                 build_reverse boolean not null, weighted boolean not null, \
                 has_relations boolean not null, retain_versions integer not null, \
                 current_version bigint"
            )
        );
        Ok(())
    }

    #[pg_test]
    fn versions_table_matches_the_design() -> Result<(), Error> {
        assert_eq!(
            columns("versions")?.as_deref(),
            Some(
                "version_id bigint not null, graph_id integer not null, seq bigint not null, \
                 staged boolean not null, built_at timestamp with time zone not null, \
                 built_lsn pg_lsn not null, node_count bigint not null, \
                 id_space bigint not null, edge_count bigint not null, \
                 dropped_edges bigint not null, deduplicated_edges bigint not null, \
                 target_width smallint not null, compacted_from bigint, \
                 bytes bigint not null, label text, parent_version bigint, label_vector bigint"
            )
        );
        Ok(())
    }

    #[pg_test]
    fn segment_and_vector_tables_match_the_design() -> Result<(), Error> {
        assert_eq!(
            columns("segments")?.as_deref(),
            Some(
                "version_id bigint not null, segment text not null, chunk_no integer not null, \
                 data bytea not null"
            )
        );
        assert_eq!(
            columns("vectors")?.as_deref(),
            Some(
                "vector_id bigint not null, graph_id integer not null, \
                 version_id bigint not null, name text not null, dtype text not null, \
                 width integer not null, length bigint not null, \
                 created_at timestamp with time zone not null, created_by regrole not null"
            )
        );
        assert_eq!(
            columns("vector_chunks")?.as_deref(),
            Some("vector_id bigint not null, chunk_no integer not null, data bytea not null")
        );
        Ok(())
    }

    #[pg_test]
    fn segment_and_vector_data_are_stored_external_uncompressed() -> Result<(), Error> {
        let storage = Spi::get_one::<String>(
            "SELECT string_agg(attrelid::regclass::text || ':' || attstorage::text, ' ' ORDER BY attrelid::regclass::text)
             FROM pg_attribute WHERE attname = 'data'
               AND attrelid IN ('warren.segments'::regclass, 'warren.vector_chunks'::regclass)",
        )?;
        assert_eq!(storage.as_deref(), Some("warren.segments:e warren.vector_chunks:e"));
        Ok(())
    }

    #[pg_test]
    fn public_has_no_privilege_on_any_catalog_table_or_sequence() -> Result<(), Error> {
        let granted_to_public = Spi::get_one::<i64>(
            "SELECT count(*) FROM pg_class c,
                    aclexplode(coalesce(c.relacl, acldefault('r', c.relowner))) a
             WHERE c.relnamespace = 'warren'::regnamespace AND c.relkind IN ('r', 'S')
               AND a.grantee = 0",
        )?;
        assert_eq!(granted_to_public, Some(0));
        Ok(())
    }

    #[pg_test]
    fn every_table_and_sequence_is_dumped_with_the_extension() -> Result<(), Error> {
        let dumped = Spi::get_one::<String>(
            "SELECT string_agg(c::regclass::text, ', ' ORDER BY c::regclass::text)
             FROM pg_extension e, unnest(e.extconfig) c WHERE e.extname = 'pg_warren'",
        )?;
        assert_eq!(
            dumped.as_deref(),
            Some(
                "warren.graphs, warren.graphs_graph_id_seq, warren.segments, \
                 warren.vector_chunks, warren.vectors, warren.vectors_vector_id_seq, \
                 warren.versions, warren.versions_version_id_seq"
            )
        );
        let unregistered = Spi::get_one::<i64>(
            "SELECT count(*) FROM pg_class c
             WHERE c.relnamespace = 'warren'::regnamespace AND c.relkind IN ('r', 'S')
               AND c.oid <> ALL ((SELECT extconfig FROM pg_extension
                                  WHERE extname = 'pg_warren')::oid[])",
        )?;
        assert_eq!(unregistered, Some(0));
        Ok(())
    }
}
