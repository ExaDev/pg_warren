//! In-database tests of `warren.gather`.

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, rows, small};

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
}
