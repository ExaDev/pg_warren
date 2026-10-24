//! In-database tests of `warren.gather`.

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use warren_core::compute::{Bytes, Messages};
    use warren_core::graph::FixedKeys;

    use crate::cache::resolve;
    use crate::compute_sql::gather::read;
    use crate::compute_sql::{entry, Vectors};
    use crate::fixture::{allocated, failure, refresh, relation_expansions, ring, rows, small};
    use crate::source::with_pinned_snapshot;

    /// `warren.gather` over the small graph, as `node:value` text.
    fn gathered(messages: &str, value_type: &str, options: &str) -> Result<Option<String>, Error> {
        rows(&format!(
            "SELECT node_id || ':' || value::text
             FROM warren.gather('t', $m${messages}$m$, NULL::bigint, NULL::{value_type}{options})"
        ))
    }

    // The small graph: 1 → 2 (a), 2 → 3 (a), 3 → 4 (b), 1 → 4 (b), 4 → 1 (a),
    // 5 → 5 (b); 6 isolated. In-neighbours: 1 ← 4, 2 ← 1, 3 ← 2, 4 ← 1, 3,
    // 5 ← 5.
    const TENS: &str = "SELECT id, id * 10 FROM t_nodes";

    #[pg_test]
    fn collect_lists_each_receivers_messages_by_sender() -> Result<(), Error> {
        small("t")?;
        assert_eq!(
            gathered(TENS, "bigint[]", "")?.as_deref(),
            Some("1:{40} 2:{10} 3:{20} 4:{10,30} 5:{50}")
        );
        let out = gathered(TENS, "bigint[]", ", direction := 'out'")?;
        assert_eq!(out.as_deref(), Some("1:{20,40} 2:{30} 3:{40} 4:{10} 5:{50}"));
        let both = gathered(TENS, "bigint[]", ", direction := 'both'")?;
        assert_eq!(both.as_deref(), Some("1:{20,40} 2:{10,30} 3:{20,40} 4:{10,30} 5:{50}"));
        Ok(())
    }

    #[pg_test]
    fn numeric_combiners_return_sql_typed_values() -> Result<(), Error> {
        small("t")?;
        assert_eq!(
            gathered(TENS, "bigint", ", 'sum'")?.as_deref(),
            Some("1:40 2:10 3:20 4:40 5:50")
        );
        assert_eq!(gathered(TENS, "bigint", ", 'count'")?.as_deref(), Some("1:1 2:1 3:1 4:2 5:1"));
        let narrow = "SELECT id, (id * 10)::int FROM t_nodes";
        assert_eq!(
            gathered(narrow, "integer", ", 'max'")?.as_deref(),
            Some("1:40 2:10 3:20 4:30 5:50")
        );
        assert_eq!(
            gathered(narrow, "bigint", ", 'sum'")?.as_deref(),
            Some("1:40 2:10 3:20 4:40 5:50")
        );
        let halves = "SELECT id, id / 2.0::float8 FROM t_nodes";
        assert_eq!(
            gathered(halves, "double precision", ", 'min'")?.as_deref(),
            Some("1:2 2:0.5 3:1 4:0.5 5:2.5")
        );
        let odd = "SELECT id, id % 2 = 1 FROM t_nodes";
        assert_eq!(
            gathered(odd, "boolean", ", 'bool_and'")?.as_deref(),
            Some("1:false 2:true 3:false 4:true 5:true")
        );
        Ok(())
    }

    #[pg_test]
    fn mode_takes_the_most_frequent_message_and_include_self_counts_the_receivers_own(
    ) -> Result<(), Error> {
        small("t")?;
        // Odd nodes send 1, even nodes 0.
        let parity = "SELECT id, (id % 2)::int FROM t_nodes";
        let both = gathered(parity, "integer", ", 'mode', 'both'")?;
        assert_eq!(both.as_deref(), Some("1:0 2:1 3:0 4:1 5:1"));
        // With its own message each of 1, 2 and 3 has a tie, which the
        // smaller takes; 6 has no senders but its own message.
        let own = gathered(parity, "integer", ", 'mode', include_self := true")?;
        assert_eq!(own.as_deref(), Some("1:0 2:0 3:0 4:1 5:1 6:0"));
        let collected = gathered(TENS, "bigint[]", ", include_self := true")?;
        assert_eq!(
            collected.as_deref(),
            Some("1:{10,40} 2:{20,10} 3:{30,20} 4:{40,10,30} 5:{50,50} 6:{60}")
        );
        let text = failure("warren.gather('t', 'SELECT id, id::text FROM t_nodes', NULL::bigint, NULL::text, 'mode')")?;
        assert_eq!(text.as_deref(), Some("42804: combine := mode does not accept text messages"));
        let null =
            failure("warren.gather('t', 'x', NULL::bigint, NULL::bigint, include_self := NULL)")?;
        assert_eq!(null.as_deref(), Some("22004: include_self must not be NULL"));
        Ok(())
    }

    /// With `per_relation`, a sender sends once in each relation linking
    /// it to the receiver (both directions of one relation once), with or
    /// without a stored merged CSR and a reverse CSR; by default once.
    #[pg_test]
    fn per_relation_a_sender_sends_once_in_each_of_its_relations() -> Result<(), Error> {
        relation_expansions("gp")?;
        Spi::run(
            "SELECT warren.create_graph('gp_f', 'SELECT id FROM gp_nodes',
                                        'SELECT src, dst, rel FROM gp_edges', has_relations := true,
                                        build_reverse := false);
             SELECT warren.refresh('gp_f')",
        )?;
        for graph in ["gp_s", "gp_u", "gp_f"] {
            for direction in ["in", "out", "both"] {
                for per_relation in [false, true] {
                    let relation = if per_relation { "rel" } else { "''" };
                    let expected = rows(&format!(
                        "SELECT receiver || ':' || count(*) || ':' || array_agg(sender ORDER BY sender)::text
                         FROM (SELECT dst AS receiver, {relation} AS r, src AS sender FROM gp_edges
                               WHERE '{direction}' IN ('in', 'both')
                               UNION SELECT src, {relation}, dst FROM gp_edges
                               WHERE '{direction}' IN ('out', 'both')) l
                         GROUP BY receiver ORDER BY receiver"
                    ))?;
                    let found = rows(&format!(
                        "SELECT c.node_id || ':' || n.value || ':' || c.value::text
                         FROM warren.gather('{graph}', 'SELECT id, id FROM gp_nodes', NULL::bigint,
                                            NULL::bigint[], direction := '{direction}',
                                            per_relation := {per_relation}) c
                         JOIN warren.gather('{graph}', 'SELECT id, id FROM gp_nodes', NULL::bigint,
                                            NULL::bigint, 'count', '{direction}',
                                            per_relation := {per_relation}) n USING (node_id)
                         ORDER BY c.node_id"
                    ))?;
                    assert_eq!(found, expected, "{graph} {direction} {per_relation}");
                }
            }
        }
        let null = failure(
            "warren.gather('gp_s', 'x', NULL::bigint, NULL::bigint, per_relation := NULL)",
        )?;
        assert_eq!(null.as_deref(), Some("22004: per_relation must not be NULL"));
        Ok(())
    }

    #[pg_test]
    fn a_call_through_the_0_4_0_definition_does_not_include_self() -> Result<(), Error> {
        small("t")?;
        // Between installing this library and `ALTER EXTENSION pg_warren
        // UPDATE`, 0.4.0's definition, without include_self, calls it.
        Spi::run(
            "CREATE SCHEMA released;
             CREATE FUNCTION released.gather(graph text, messages text, id_type anyelement,
                                             value_type anycompatible,
                                             combine text DEFAULT 'collect',
                                             direction text DEFAULT 'in', rels text[] DEFAULT NULL,
                                             mask text DEFAULT NULL,
                                             mask_complement boolean DEFAULT false,
                                             version bigint DEFAULT NULL)
             RETURNS TABLE (node_id anyelement, value anycompatible)
             VOLATILE PARALLEL UNSAFE LANGUAGE c AS 'pg_warren', 'gather_wrapper'",
        )?;
        let released = rows(&format!(
            "SELECT node_id || ':' || value::text
             FROM released.gather('t', $m${TENS}$m$, NULL::bigint, NULL::bigint[])"
        ))?;
        assert_eq!(released, gathered(TENS, "bigint[]", "")?);
        Ok(())
    }

    #[pg_test]
    fn any_type_travels_and_text_concatenates() -> Result<(), Error> {
        small("t")?;
        let names = "SELECT id, 'n' || id FROM t_nodes";
        assert_eq!(
            gathered(names, "text[]", "")?.as_deref(),
            Some("1:{n4} 2:{n1} 3:{n2} 4:{n1,n3} 5:{n5}")
        );
        assert_eq!(
            gathered(names, "text", ", 'concat'")?.as_deref(),
            Some("1:n4 2:n1 3:n2 4:n1n3 5:n5")
        );
        assert_eq!(
            gathered(names, "text", ", 'first'")?.as_deref(),
            Some("1:n4 2:n1 3:n2 4:n1 5:n5")
        );
        let json = r#"SELECT id, jsonb_build_object('id', id) FROM t_nodes WHERE id IN (1, 3)"#;
        assert_eq!(
            gathered(json, "jsonb[]", "")?.as_deref(),
            Some(r#"2:{"{\"id\": 1}"} 4:{"{\"id\": 1}","{\"id\": 3}"}"#)
        );
        let pairs = "SELECT id, ARRAY[id, -id] FROM t_nodes";
        assert_eq!(
            gathered(pairs, "bigint[]", ", rels := ARRAY['b']")?.as_deref(),
            Some("4:{{1,-1},{3,-3}} 5:{{5,-5}}")
        );
        Ok(())
    }

    #[pg_test]
    fn several_messages_per_sender_keep_their_order_and_nulls_are_skipped() -> Result<(), Error> {
        small("t")?;
        let many =
            "SELECT id, v FROM t_nodes, LATERAL (VALUES (id * 10), (NULL), (id * 10 + 1)) m(v)
                    UNION ALL SELECT NULL, 7 UNION ALL SELECT 99, 7";
        assert_eq!(
            gathered(many, "bigint[]", ", rels := ARRAY['a']")?.as_deref(),
            Some("1:{40,41} 2:{10,11} 3:{20,21}")
        );
        Ok(())
    }

    #[pg_test]
    fn vectors_and_relations_are_message_sources_too() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "SELECT warren.vector_from_query('t', 'x', 'SELECT id, id FROM t_nodes', 'i64');
             CREATE TABLE t_messages (node bigint, label text);
             INSERT INTO t_messages VALUES (1, 'one'), (3, 'three')",
        )?;
        assert_eq!(gathered("x", "bigint", ", 'sum'")?.as_deref(), Some("1:4 2:1 3:2 4:4 5:5"));
        assert_eq!(gathered("t_messages", "text[]", "")?.as_deref(), Some("2:{one} 4:{one,three}"));
        assert_eq!(
            gathered("public.t_messages", "bigint", ", 'count'")?.as_deref(),
            Some("2:1 4:2")
        );
        Ok(())
    }

    #[pg_test]
    fn a_vector_never_shadows_a_query_and_a_name_of_both_is_ambiguous() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "CREATE TABLE msgs AS SELECT id, id * 100 AS v FROM t_nodes;
             SELECT warren.vector_const('t', 'SELECT id, v FROM msgs', 2, 'i64');
             SELECT warren.vector_const('t', 'only_a_vector', 3, 'i64')",
        )?;
        let query = gathered("SELECT id, v FROM msgs", "bigint", ", 'sum'")?;
        assert_eq!(query.as_deref(), Some("1:400 2:100 3:200 4:400 5:500"));
        let vector = gathered("only_a_vector", "bigint", ", 'sum'")?;
        assert_eq!(vector.as_deref(), Some("1:3 2:3 3:3 4:6 5:3"));
        assert_eq!(
            gathered("msgs", "bigint", ", 'sum'")?.as_deref(),
            Some("1:400 2:100 3:200 4:400 5:500")
        );
        Spi::run("SELECT warren.vector_const('t', 'msgs', 1, 'i64')")?;
        let ambiguous = failure("warren.gather('t', 'msgs', NULL::bigint, NULL::bigint, 'sum')")?
            .unwrap_or_default();
        assert!(
            ambiguous.starts_with("42725: messages \"msgs\" names both a vector bound to version"),
            "{ambiguous}"
        );
        let table = gathered("TABLE msgs", "bigint", ", 'sum'")?;
        assert_eq!(table.as_deref(), Some("1:400 2:100 3:200 4:400 5:500"));
        Ok(())
    }

    #[pg_test]
    fn results_stream_row_by_row() -> Result<(), Error> {
        ring("big", 20_000, 2)?;
        refresh("big", true)?;
        let call = "warren.gather('big', 'SELECT id, jsonb_build_array(id, id) FROM big_nodes, generate_series(1, 3)',
                                  NULL::bigint, NULL::jsonb[])";
        let first = rows(&format!("SELECT node_id || ':' || value::text FROM {call} LIMIT 1"))?;
        assert_eq!(
            first.as_deref(),
            Some("1:{\"[20000, 20000]\",\"[20000, 20000]\",\"[20000, 20000]\"}")
        );
        let all = Spi::get_one::<i64>(&format!(
            "SELECT count(*) FROM {call} WHERE cardinality(value) = 3"
        ))?;
        assert_eq!(all, Some(20_000));
        Ok(())
    }

    #[pg_test]
    fn concat_joins_bytes_whatever_their_encoding() -> Result<(), Error> {
        small("t")?;
        let bytes = "SELECT id, CASE WHEN id = 1 THEN '\\xff00'::bytea ELSE '\\x41'::bytea END FROM t_nodes";
        let joined = gathered(bytes, "bytea", ", 'concat', direction := 'out'")?;
        assert_eq!(joined.as_deref(), Some("1:\\x4141 2:\\x41 3:\\x41 4:\\xff00 5:\\x41"));
        Ok(())
    }

    #[pg_test]
    fn messages_cannot_write() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "CREATE TABLE t_log (n int);
             CREATE FUNCTION t_write() RETURNS bigint VOLATILE LANGUAGE plpgsql AS
                 $f$ BEGIN INSERT INTO t_log VALUES (1); RETURN 1; END $f$",
        )?;
        let cte = failure(
            "warren.gather('t', 'WITH d AS (DELETE FROM t_nodes RETURNING id) SELECT id, id FROM d',
                           NULL::bigint, NULL::bigint[])",
        )?
        .unwrap_or_default();
        assert!(
            cte.starts_with("0A000: WITH clause containing a data-modifying statement"),
            "{cte}"
        );
        let function = failure(
            "warren.gather('t', 'SELECT id, t_write() FROM t_nodes', NULL::bigint, NULL::bigint[])",
        )?
        .unwrap_or_default();
        assert!(
            function.starts_with("25006: cannot execute INSERT in a read-only transaction"),
            "{function}"
        );
        let written = Spi::get_one::<i64>(
            "SELECT (SELECT count(*) FROM t_log) + 6 - (SELECT count(*) FROM t_nodes)",
        )?;
        assert_eq!(written, Some(0));
        Ok(())
    }

    #[pg_test]
    fn relations_resolve_on_the_callers_search_path_with_the_callers_privileges(
    ) -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "CREATE SCHEMA hidden;
             CREATE TABLE hidden.inbox AS SELECT id, id * 3 AS v FROM t_nodes;
             CREATE ROLE t_reader;
             SELECT warren.grant('t', 't_reader');
             GRANT USAGE ON SCHEMA hidden TO t_reader",
        )?;
        // The failure helper's table, writable by the reader too.
        failure("1")?;
        Spi::run("GRANT ALL ON failure TO t_reader")?;
        let unseen = failure("warren.gather('t', 'inbox', NULL::bigint, NULL::bigint, 'sum')")?
            .unwrap_or_default();
        assert!(unseen.starts_with("42P01: relation \"inbox\" does not exist"), "{unseen}");
        Spi::run("SET LOCAL search_path = hidden, public")?;
        assert_eq!(
            gathered("inbox", "bigint", ", 'sum'")?.as_deref(),
            Some("1:12 2:3 3:6 4:12 5:15")
        );
        Spi::run("SET LOCAL ROLE t_reader")?;
        let refused = failure("warren.gather('t', 'inbox', NULL::bigint, NULL::bigint, 'sum')")?
            .unwrap_or_default();
        assert!(refused.starts_with("42501: permission denied for table inbox"), "{refused}");
        Spi::run("RESET ROLE; GRANT SELECT ON hidden.inbox TO t_reader; SET LOCAL ROLE t_reader")?;
        assert_eq!(
            gathered("inbox", "bigint", ", 'sum'")?.as_deref(),
            Some("1:12 2:3 3:6 4:12 5:15")
        );
        Spi::run("RESET ROLE")?;
        Ok(())
    }

    #[pg_test]
    fn a_non_reader_of_the_graph_cannot_gather() -> Result<(), Error> {
        small("t")?;
        failure("1")?;
        Spi::run(
            "CREATE ROLE t_stranger; GRANT SELECT ON t_nodes TO t_stranger; GRANT ALL ON failure TO t_stranger;
             SET LOCAL ROLE t_stranger",
        )?;
        let refused = failure(
            "warren.gather('t', 'SELECT id, id FROM t_nodes', NULL::bigint, NULL::bigint[])",
        )?
        .unwrap_or_default();
        Spi::run("RESET ROLE")?;
        assert!(refused.starts_with("42501: must be a member of role \"t_owner\" or of a reader role to read graph \"t\""), "{refused}");
        Ok(())
    }

    #[pg_test]
    fn a_mask_restricts_the_receivers() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT warren.vector_from_query('t', 'odd', 'SELECT id, id % 2 FROM t_nodes')")?;
        assert_eq!(
            gathered(TENS, "bigint", ", 'sum', mask := 'odd'")?.as_deref(),
            Some("1:40 3:20 5:50")
        );
        let even = ", 'sum', mask := 'odd', mask_complement := true";
        assert_eq!(gathered(TENS, "bigint", even)?.as_deref(), Some("2:10 4:40"));
        Ok(())
    }

    #[pg_test]
    fn bad_gathers_are_errors() -> Result<(), Error> {
        small("t")?;
        let call = |args: &str| failure(&format!("warren.gather('t', {args})"));
        let cases = [
            (
                format!("'{TENS}', NULL::bigint, NULL::bigint"),
                "42804: gather with combine := collect over bigint messages returns bigint[]",
            ),
            (format!("'{TENS}', NULL::int, NULL::bigint[]"), "42804: graph \"t\" has node ids of type bigint, but id_type"),
            (
                "'SELECT id, id::text FROM t_nodes', NULL::bigint, NULL::text, 'sum'".to_owned(),
                "42804: combine := sum does not accept text messages",
            ),
            (format!("'{TENS}', NULL::bigint, NULL::bigint, 'avg'"), "22023: unknown combiner \"avg\""),
            ("'SELECT id FROM t_nodes', NULL::bigint, NULL::bigint[]".to_owned(), "42804: messages must return 2 (id, message) columns, not 1"),
            (
                "'SELECT id::float8, 1 FROM t_nodes', NULL::bigint, NULL::int[]".to_owned(),
                "42804: query column 1 (id) is of type double precision",
            ),
            (
                "'SELECT id, ARRAY[id] FROM t_nodes UNION ALL SELECT 2, ARRAY[1, 2]', NULL::bigint, NULL::bigint[]".to_owned(),
                "2202E: cannot accumulate arrays of different dimensionality",
            ),
            (format!("'{TENS}', NULL::bigint, NULL::bigint[], direction := 'up'"), "22023: direction must be"),
        ];
        for (args, expected) in cases {
            let found = call(&args)?.unwrap_or_default();
            assert!(found.starts_with(expected), "{args}: {found}");
        }
        Ok(())
    }

    /// Reading messages holds no Postgres memory per row: what a row's
    /// send function allocates (an array's detoasted copy and I/O state)
    /// is freed with its batch, not held until the read ends.
    #[pg_test]
    fn reading_messages_holds_no_memory_per_row() -> Result<(), Error> {
        ring("big", 20_000, 2)?;
        refresh("big", true)?;
        Spi::run(
            "CREATE TABLE big_messages AS
                 SELECT (g % 20000 + 1)::bigint AS id, ARRAY[g, g]::int[] AS m
                 FROM generate_series(1, 100000) g",
        )?;
        let (mut send, mut varlena) = (pg_sys::Oid::INVALID, false);
        // SAFETY: the out-pointers point to live locals.
        unsafe { pg_sys::getTypeBinaryOutputInfo(pg_sys::INT4ARRAYOID, &mut send, &mut varlena) };
        let (rows, first_batch, peak) = with_pinned_snapshot(|| {
            entry(|caller| {
                let vectors = Vectors::<FixedKeys<i64>>::open(resolve("big", None, "read")?)?;
                let mut messages = Messages::<Bytes>::new(&vectors.memory)?;
                let (before, mut rows, mut first_batch, mut peak) = (allocated(), 0, 0, 0);
                read::from_query(
                    &vectors,
                    caller,
                    "SELECT id, m FROM big_messages",
                    |node, row| {
                        read::binary(send, &mut messages, node, row)?;
                        peak = peak.max(allocated().saturating_sub(before));
                        rows += 1;
                        if rows == 10_000 {
                            first_batch = peak;
                        }
                        Ok(())
                    },
                )?;
                Ok((rows, first_batch, peak))
            })
        });
        assert_eq!(rows, 100_000);
        assert!(
            peak <= first_batch + (256 << 10),
            "{peak} bytes allocated while reading, {first_batch} after the first batch"
        );
        Ok(())
    }
}
