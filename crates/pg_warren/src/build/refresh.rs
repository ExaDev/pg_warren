//! `warren.refresh`: builds a new version of a graph from its sources under
//! one pinned snapshot, and promotes it unless asked not to (DESIGN.md §5.4,
//! §7). This is where the SPI adapters are wired to the build pipeline.

use pgrx::prelude::*;

use super::budget::max_build_memory;
use super::error::BuildError;
use super::pipeline::{build, RefreshRequest};
use super::spi_catalog::SpiCatalog;
use super::spi_source::SpiSource;
use crate::access::{entry, require_owner};
use crate::activity::Running;
use crate::catalog::{require_current_id_space, GraphDefinition, VersionId};
use crate::interrupts::{workers, PostgresInterrupts};
use crate::segment::{SpiSegmentStore, CHUNK_SIZE};
use crate::source::{with_id_type, with_pinned_snapshot};

/// `warren.refresh`: builds a new version of graph `name` and returns its
/// id. The caller must be a member of the graph's owner role. The version
/// is made current if `promote`, and is otherwise left staged.
#[pg_extern(schema = "warren")]
fn refresh(
    name: &str,
    promote: default!(bool, true),
    label: default!(Option<&str>, "NULL"),
) -> i64 {
    entry(|_| refresh_graph(name, promote, label))
}

fn refresh_graph(name: &str, promote: bool, label: Option<&str>) -> Result<VersionId, BuildError> {
    let mut graph = GraphDefinition::find(name)?;
    require_owner(graph.owner, &graph.name, "refresh")?;
    graph.require_query_source("refresh")?;
    graph.lock("refresh")?;
    graph.require_sound_current_version()?;
    if let Some(base) = graph.current_version {
        require_current_id_space(&graph, base)?;
    }
    let _running = Running::start("refresh", Some(graph.graph_id), None);
    let built = with_pinned_snapshot(|| {
        let request = RefreshRequest {
            promote,
            label,
            chunk_size: CHUNK_SIZE,
            memory_budget: max_build_memory(),
        };
        with_id_type!(graph.id_type, E => build::<E>(
            &graph,
            &request,
            &mut SpiSource::new(&graph),
            (&*workers()?, &mut PostgresInterrupts),
            &mut SpiSegmentStore,
            &mut SpiCatalog::at_current_wal_lsn()?,
        ))
    })?;
    debug1!("refresh of {} held at most {} bytes of build memory", graph.name, built.memory_peak);
    Ok(built.version)
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;
    use warren_core::graph::{
        AnyCsr, BuildMemory, EdgeIntake, FixedKeys, GraphCsr, IdMap, RelationDictionary, TextKeys,
        Tombstones,
    };

    use crate::fixture;
    use crate::segment::{read_segment, SpiSegmentStore, StoredIds};

    /// A ring of 200 nodes, 1 → 2 → … → 200 → 1, as graph `ring`.
    fn ring() -> Result<(), Error> {
        fixture::ring("ring", 200, 2)
    }

    fn refresh(sql: &str) -> Result<i64, Error> {
        Spi::get_one::<i64>(sql)?.ok_or(Error::InvalidPosition)
    }

    fn segment(version: i64, name: &str) -> Vec<u8> {
        read_segment(&mut SpiSegmentStore, version, name).unwrap_or_default()
    }

    fn stored_id_map(version: i64) -> Option<IdMap<FixedKeys<i64>>> {
        let tombstones = Tombstones::decode(&segment(version, "tombstones")).ok()?;
        IdMap::decode(&segment(version, "id_map"), tombstones).ok()
    }

    fn version_row(version: i64) -> Result<Option<String>, Error> {
        Spi::get_one_with_args::<String>(
            "SELECT format('%s|%s|%s|%s|%s|%s|%s|%s|%s|%s', seq, staged, node_count, id_space,
                           edge_count, dropped_edges, deduplicated_edges, target_width,
                           bytes = (SELECT sum(length(data)) FROM warren.segments s
                                    WHERE s.version_id = v.version_id),
                           label)
             FROM warren.versions v WHERE version_id = $1",
            &[version.into()],
        )
    }

    /// DESIGN.md §5.1: a refresh must not build on a current version that
    /// is not one of the graph's (another graph's id map, or none); promote
    /// is the remedy.
    #[pg_test]
    fn refresh_refuses_a_stray_current_version_until_one_is_promoted() -> Result<(), Error> {
        use crate::fixture::{failure, set_current_unchecked, stray_current_failure, two_graphs};
        let (own, other) = two_graphs("rstray")?;
        for stray in [other, -1] {
            set_current_unchecked("rstray_a", stray)?;
            assert_eq!(
                failure("warren.refresh('rstray_a')")?,
                Some(stray_current_failure("rstray_a", stray))
            );
        }
        Spi::run(&format!("SELECT warren.promote('rstray_a', {own})"))?;
        assert_eq!(failure("warren.refresh('rstray_a')")?, None);
        Ok(())
    }

    #[pg_test]
    fn refresh_builds_and_promotes_a_version() -> Result<(), Error> {
        ring()?;
        let version = refresh("SELECT warren.refresh('ring', label := 'first')")?;
        assert_eq!(version_row(version)?.as_deref(), Some("1|f|200|200|200|0|0|4|t|first"));
        let current = Spi::get_one::<i64>("SELECT current_version FROM warren.graphs")?;
        assert_eq!(current, Some(version));
        let id_map_chunks = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM warren.segments WHERE version_id = $1 AND segment = 'id_map'",
            &[version.into()],
        )?;
        assert_eq!(id_map_chunks, Some(4), "200 × 16 bytes span four 1 KiB chunks");
        Ok(())
    }

    #[pg_test]
    fn a_staged_refresh_does_not_become_current() -> Result<(), Error> {
        ring()?;
        let version = refresh("SELECT warren.refresh('ring', promote := false)")?;
        assert_eq!(version_row(version)?.as_deref(), Some("1|t|200|200|200|0|0|4|t|"));
        let current = Spi::get_one::<i64>("SELECT current_version FROM warren.graphs")?;
        assert_eq!(current, None);
        Ok(())
    }

    #[pg_test]
    fn segments_decode_back_to_the_same_csr() -> Result<(), Error> {
        ring()?;
        let version = refresh("SELECT warren.refresh('ring')")?;

        let id_map = IdMap::new().rebuild((1..=200).collect()).unwrap_or_default();
        let mut relations = RelationDictionary::new();
        let relation = relations.intern("").map_err(|_| Error::InvalidPosition)?;
        let mut intake = EdgeIntake::directed(&id_map, false);
        for source in 1..=200i64 {
            let target = source % 200 + 1;
            intake
                .push(relation, Some(&source), Some(&target), 1.0)
                .map_err(|_| Error::InvalidPosition)?;
        }
        let expected =
            GraphCsr::build(intake.finish().0, 1, 200, true).map_err(|_| Error::InvalidPosition)?;

        assert_eq!(stored_id_map(version), Some(id_map));
        assert_eq!(RelationDictionary::decode(&segment(version, "rel_dict")).ok(), Some(relations));
        let decode = |prefix: &str| {
            AnyCsr::decode(
                200,
                &segment(version, &format!("{prefix}offsets:0")),
                &segment(version, &format!("{prefix}targets:0")),
                None,
            )
            .ok()
        };
        assert_eq!(decode("").as_ref(), Some(expected.relations()[0].forward()));
        assert_eq!(decode("rev_").as_ref(), expected.relations()[0].reverse());
        Ok(())
    }

    #[pg_test]
    fn dropped_and_deduplicated_edges_are_counted() -> Result<(), Error> {
        ring()?;
        Spi::run(
            "INSERT INTO ring_edges VALUES (1, 2), (1, 2), (3, 4),
                                          (1, 999), (NULL, 5), (6, NULL)",
        )?;
        let version = refresh("SELECT warren.refresh('ring')")?;
        assert_eq!(version_row(version)?.as_deref(), Some("1|f|200|200|200|3|3|4|t|"));
        Ok(())
    }

    #[pg_test]
    fn relations_and_weights_are_read_from_the_edge_query() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE typed_nodes (id bigint);
             CREATE TABLE typed_edges (src bigint, dst bigint, rel text, w float8);
             INSERT INTO typed_nodes VALUES (1), (2), (3);
             INSERT INTO typed_edges VALUES (1, 2, 'cites', 0.5), (1, 2, 'cites', 0.25),
                                            (1, 3, 'extends', 2), (2, 3, NULL, 1),
                                            (2, 3, 'cites', NULL);
             SELECT warren.create_graph('typed', 'SELECT id FROM typed_nodes',
                 'SELECT src, dst, rel, w FROM typed_edges', weighted := true,
                 has_relations := true)",
        )?;
        let version = refresh("SELECT warren.refresh('typed')")?;
        assert_eq!(version_row(version)?.as_deref(), Some("1|f|3|3|2|2|1|4|t|"));
        let relations = RelationDictionary::decode(&segment(version, "rel_dict")).ok();
        assert_eq!(
            relations.map(|r| r.names().to_vec()),
            Some(vec!["cites".into(), "extends".into()])
        );
        let cites = AnyCsr::decode(
            3,
            &segment(version, "offsets:0"),
            &segment(version, "targets:0"),
            Some(&segment(version, "weights:0")),
        )
        .ok();
        assert_eq!(
            cites.and_then(|c| c.neighbour_weights(0).map(<[f64]>::to_vec)),
            Some(vec![0.75])
        );
        let merged = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM warren.segments WHERE version_id = $1 AND segment LIKE '%:merged'",
            &[version.into()],
        )?;
        assert_eq!(merged, Some(6), "forward and reverse offsets, targets and weights");
        Ok(())
    }

    #[pg_test]
    fn int4_and_uuid_graphs_build_with_stable_ids() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE small_nodes (id int);
             CREATE TABLE small_edges (src int, dst int);
             INSERT INTO small_nodes VALUES (3), (1), (2);
             INSERT INTO small_edges VALUES (1, 2), (2, 3), (3, 9);
             SELECT warren.create_graph('small', 'SELECT id FROM small_nodes',
                                        'SELECT src, dst FROM small_edges');
             CREATE TABLE u_nodes (id uuid);
             CREATE TABLE u_edges (src uuid, dst uuid);
             INSERT INTO u_nodes VALUES ('00000000-0000-0000-0000-000000000002'),
                                        ('00000000-0000-0000-0000-000000000001');
             INSERT INTO u_edges VALUES ('00000000-0000-0000-0000-000000000001',
                                         '00000000-0000-0000-0000-000000000002');
             SELECT warren.create_graph('u', 'SELECT id FROM u_nodes',
                                        'SELECT src, dst FROM u_edges')",
        )?;
        let small = refresh("SELECT warren.refresh('small')")?;
        assert_eq!(version_row(small)?.as_deref(), Some("1|f|3|3|2|1|0|4|t|"));
        let uuid = refresh("SELECT warren.refresh('u')")?;
        assert_eq!(version_row(uuid)?.as_deref(), Some("1|f|2|2|1|0|0|4|t|"));
        let tombstones = Tombstones::decode(&segment(uuid, "tombstones")).ok();
        let id_map = tombstones
            .and_then(|t| IdMap::<FixedKeys<[u8; 16]>>::decode(&segment(uuid, "id_map"), t).ok());
        let mut first = [0; 16];
        first[15] = 1;
        assert_eq!(id_map.and_then(|m| m.dense_id(&first)), Some(0));
        Spi::run("INSERT INTO small_nodes VALUES (0)")?;
        let next = refresh("SELECT warren.refresh('small')")?;
        assert_eq!(version_row(next)?.as_deref(), Some("2|f|4|4|2|1|0|4|t|"));
        Ok(())
    }

    /// DESIGN.md §5.7: a text graph's ids are its id map's three
    /// segments, in bytewise order, and keep their dense ids across
    /// refreshes; an id is any bytes, the empty one included.
    #[pg_test]
    fn text_graphs_build_with_stable_ids() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE t_nodes (id text);
             CREATE TABLE t_edges (src varchar, dst text);
             INSERT INTO t_nodes VALUES ('b'), (''), ('é'), ('B');
             INSERT INTO t_edges VALUES ('', 'b'), ('b', 'é'), ('é', 'unknown'), (NULL, 'b');
             SELECT warren.create_graph('t', 'SELECT id FROM t_nodes',
                                        'SELECT src, dst FROM t_edges')",
        )?;
        let first = refresh("SELECT warren.refresh('t')")?;
        assert_eq!(version_row(first)?.as_deref(), Some("1|f|4|4|2|2|0|4|t|"));
        let segments = Spi::get_one_with_args::<String>(
            "SELECT string_agg(DISTINCT segment, ' ' ORDER BY segment) FROM warren.segments
             WHERE version_id = $1 AND segment LIKE 'id%'",
            &[first.into()],
        )?;
        assert_eq!(segments.as_deref(), Some("id_bytes id_offsets id_order"));
        let id_map = |version| {
            let store = &mut SpiSegmentStore;
            let tombstones = read_segment(store, version, "tombstones").unwrap_or_default();
            let memory = BuildMemory::unlimited();
            TextKeys::read_id_map(store, version, &tombstones, &memory).map(|(id_map, _)| id_map)
        };
        let ids = id_map(first).map_err(|_| Error::InvalidPosition)?;
        let order: Vec<_> = (0..4).filter_map(|dense| ids.external_id(dense)).collect();
        assert_eq!(order, [&b""[..], b"B", b"b", "é".as_bytes()]);
        Spi::run("DELETE FROM t_nodes WHERE id = 'B'; INSERT INTO t_nodes VALUES ('a')")?;
        let next = refresh("SELECT warren.refresh('t')")?;
        let ids = id_map(next).map_err(|_| Error::InvalidPosition)?;
        assert_eq!(ids.dense_id(b"b"), Some(2), "a kept id keeps its dense id");
        assert_eq!(ids.dense_id(b"a"), Some(4), "a new id is appended");
        assert_eq!(ids.dense_id(b"B"), None, "a dropped id is tombstoned");
        Ok(())
    }

    #[pg_test]
    fn a_rolled_back_staged_refresh_leaves_no_rows() -> Result<(), Error> {
        ring()?;
        // A PL/pgSQL block with an EXCEPTION clause runs in a subtransaction;
        // raising WR001 after the build rolls back exactly that subtransaction,
        // as ROLLBACK would the whole transaction (DESIGN.md §8).
        // This is a subtransaction-level rollback; a literal BEGIN; refresh;
        // ROLLBACK case needs a harness that spans transactions (test/, M3+).
        Spi::run(
            "DO $$
             BEGIN
                 PERFORM warren.refresh('ring', promote := false);
                 IF (SELECT count(*) FROM warren.versions) <> 1
                    OR (SELECT count(*) FROM warren.segments) = 0 THEN
                     RAISE EXCEPTION 'the staged build wrote no rows';
                 END IF;
                 RAISE EXCEPTION 'roll back' USING ERRCODE = 'WR001';
             EXCEPTION WHEN SQLSTATE 'WR001' THEN NULL;
             END $$",
        )?;
        let rows = Spi::get_one::<i64>(
            "SELECT (SELECT count(*) FROM warren.versions)
                  + (SELECT count(*) FROM warren.segments)
                  + (SELECT count(*) FROM warren.graphs WHERE current_version IS NOT NULL)",
        )?;
        assert_eq!(rows, Some(0));
        Ok(())
    }

    #[pg_test]
    fn dense_ids_are_stable_across_refreshes() -> Result<(), Error> {
        ring()?;
        let v1 = refresh("SELECT warren.refresh('ring')")?;
        Spi::run(
            "DELETE FROM ring_nodes WHERE id IN (2, 100);
             INSERT INTO ring_nodes VALUES (0), (500);
             INSERT INTO ring_edges VALUES (500, 0), (1, 100)",
        )?;
        let v2 = refresh("SELECT warren.refresh('ring')")?;
        let (Some(before), Some(after)) = (stored_id_map(v1), stored_id_map(v2)) else {
            return Err(Error::InvalidPosition);
        };
        for survivor in (1..=200).filter(|id| ![2, 100].contains(id)) {
            assert_eq!(after.dense_id(&survivor), before.dense_id(&survivor));
        }
        for removed in [2, 100] {
            let dense = before.dense_id(&removed);
            assert_eq!(after.dense_id(&removed), None);
            assert!(dense.is_some_and(|d| after.tombstones().contains(d)));
        }
        assert_eq!(after.dense_id(&0), Some(200));
        assert_eq!(after.dense_id(&500), Some(201));
        // 200 ring edges, 4 into or out of a removed node, plus 500 → 0;
        // 1 → 100 is dropped with them.
        assert_eq!(version_row(v2)?.as_deref(), Some("2|f|200|202|197|5|0|4|t|"));
        Ok(())
    }

    #[pg_test]
    fn refresh_sees_the_callers_uncommitted_writes() -> Result<(), Error> {
        ring()?;
        // The insert and the refresh are one statement, so the new row is
        // visible to the build only because the build advances the command
        // counter before it pins its snapshot.
        let version = refresh(
            "WITH added AS (INSERT INTO ring_nodes VALUES (201) RETURNING id)
             SELECT warren.refresh('ring') FROM added",
        )?;
        let node_count = Spi::get_one_with_args::<i64>(
            "SELECT node_count FROM warren.versions WHERE version_id = $1",
            &[version.into()],
        )?;
        assert_eq!(node_count, Some(201));
        Ok(())
    }

    #[pg_test]
    fn refresh_rechecks_the_source_queries() -> Result<(), Error> {
        ring()?;
        Spi::run("ALTER TABLE ring_edges ALTER COLUMN dst TYPE int")?;
        let failed = Spi::get_one::<bool>(
            "DO $$ BEGIN PERFORM warren.refresh('ring'); RAISE EXCEPTION 'built';
             EXCEPTION WHEN datatype_mismatch THEN NULL; END $$; SELECT true",
        );
        assert_eq!(failed, Ok(Some(true)));
        Ok(())
    }

    #[pg_test(error = "graph \"nothing\" does not exist")]
    fn refreshing_an_unknown_graph_fails() -> Result<(), Error> {
        Spi::run("SELECT warren.refresh('nothing')")
    }

    #[pg_test]
    fn a_non_owner_cannot_read_the_catalog_tables() -> Result<(), Error> {
        ring()?;
        Spi::run("SELECT warren.refresh('ring')")?;
        Spi::run(
            "CREATE ROLE catalog_snoop;
             SET ROLE catalog_snoop;
             DO $$
             DECLARE t text;
             BEGIN
                 FOREACH t IN ARRAY ARRAY['graphs', 'versions', 'segments', 'vectors',
                                          'vector_chunks'] LOOP
                     BEGIN
                         EXECUTE format('SELECT count(*) FROM warren.%I', t);
                         RAISE EXCEPTION 'warren.% is readable', t;
                     EXCEPTION WHEN insufficient_privilege THEN NULL;
                     END;
                 END LOOP;
             END $$;
             RESET ROLE",
        )
    }

    /// Graph `owned`, created by role `graph_owner` over tables it can read.
    fn owned_graph() -> Result<(), Error> {
        Spi::run(
            "CREATE ROLE graph_owner;
             CREATE TABLE owned_nodes (id bigint);
             CREATE TABLE owned_edges (src bigint, dst bigint);
             INSERT INTO owned_nodes VALUES (1), (2);
             INSERT INTO owned_edges VALUES (1, 2);
             GRANT SELECT ON owned_nodes, owned_edges TO graph_owner;
             SET ROLE graph_owner;
             SELECT warren.create_graph('owned', 'SELECT id FROM owned_nodes',
                                        'SELECT src, dst FROM owned_edges');
             RESET ROLE",
        )
    }

    #[pg_test(error = "permission denied for table owned_edges")]
    fn refresh_runs_the_source_queries_as_the_graph_owner() -> Result<(), Error> {
        owned_graph()?;
        // The caller is a superuser, but the owner can no longer read edges.
        Spi::run("REVOKE SELECT ON owned_edges FROM graph_owner")?;
        Spi::run("SELECT warren.refresh('owned')")
    }

    #[pg_test]
    fn a_member_of_the_owner_role_refreshes_with_the_owners_privileges() -> Result<(), Error> {
        owned_graph()?;
        // NOINHERIT: the member cannot read the source tables itself.
        Spi::run(
            "CREATE ROLE owner_member NOINHERIT IN ROLE graph_owner;
             SET ROLE owner_member",
        )?;
        let version = Spi::get_one::<i64>("SELECT warren.refresh('owned')");
        let readable = Spi::get_one::<bool>(
            "DO $$ BEGIN PERFORM * FROM owned_edges; RAISE EXCEPTION 'readable';
             EXCEPTION WHEN insufficient_privilege THEN NULL; END $$; SELECT true",
        );
        Spi::run("RESET ROLE")?;
        assert!(version?.is_some());
        assert_eq!(readable, Ok(Some(true)));
        Ok(())
    }

    #[pg_test(error = "must be a member of role \"graph_owner\" to refresh graph \"owned\"")]
    fn a_role_outside_the_owner_role_cannot_refresh() -> Result<(), Error> {
        owned_graph()?;
        Spi::run("CREATE ROLE outsider; SET ROLE outsider; SELECT warren.refresh('owned')")
    }

    #[pg_test(error = "the owner of graph \"owned\" no longer exists")]
    fn a_graph_whose_owner_was_dropped_cannot_refresh() -> Result<(), Error> {
        owned_graph()?;
        Spi::run(
            "REVOKE ALL ON owned_nodes, owned_edges FROM graph_owner;
             DROP ROLE graph_owner;
             SELECT warren.refresh('owned')",
        )
    }

    #[pg_test]
    fn source_queries_run_under_the_search_path_create_graph_saw() -> Result<(), Error> {
        Spi::run(
            "CREATE SCHEMA sp_a; CREATE SCHEMA sp_b;
             CREATE TABLE sp_a.n (id bigint); CREATE TABLE sp_b.n (id bigint);
             CREATE TABLE sp_a.e (src bigint, dst bigint);
             CREATE TABLE sp_b.e (src bigint, dst bigint);
             INSERT INTO sp_a.n VALUES (1), (2); INSERT INTO sp_b.n VALUES (1), (2), (3);
             SET search_path = sp_a;
             SELECT warren.create_graph('sp', 'SELECT id FROM n', 'SELECT src, dst FROM e');
             SET search_path = sp_b",
        )?;
        let version = Spi::get_one::<i64>("SELECT warren.refresh('sp')");
        Spi::run("RESET search_path")?;
        let nodes = Spi::get_one_with_args::<i64>(
            "SELECT node_count FROM warren.versions WHERE version_id = $1",
            &[version?.into()],
        )?;
        assert_eq!(nodes, Some(2), "the queries read sp_a, as when the graph was created");
        let stored =
            Spi::get_one::<String>("SELECT search_path FROM warren.graphs WHERE name = 'sp'")?;
        assert_eq!(stored.as_deref(), Some("sp_a"));
        Ok(())
    }
}
