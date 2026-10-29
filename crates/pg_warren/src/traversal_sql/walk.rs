//! `warren.walk`: a typed chain of steps executed over the CSR without
//! leaving the engine (DESIGN.md §9.2), with external ids in and out.

use std::sync::Arc;

use pgrx::prelude::*;
use pgrx::{AnyArray, AnyElement, Array};
use warren_core::traversal::{walk as run_walk, WalkMode, WalkOptions, WalkPath, WalkResult};

use super::args::bound;
use super::stats::Recorder;
use super::step::read_steps;
use super::walk_rows::{max_walk_paths, DEFAULT_WALK_ROWS};
use super::{entry, TraversalSqlError};
use crate::cache::{cached, resolve, CachedIds, LoadedVersion};
use crate::ceiling::Bound;
use crate::external_ids::{dense_ids, elements, external, external_array};
use crate::interrupts::PostgresInterrupts;
use crate::source::with_id_type;

/// A row of `warren.walk`.
type WalkRow = (Option<AnyElement>, Option<AnyArray>, Option<Vec<String>>, Option<i64>);

/// `warren.walk`: the walks along `steps` from `start` (or, with `reverse`,
/// ending in `start`). `mode` `ends` returns each end (`end_id`), once if
/// `distinct_ends`, else once per walk; `paths` returns walks with their
/// nodes (`path`) and each hop's relation (`rels`), one per distinct end or
/// every walk; `count` returns one row whose `count` is the number of ends
/// or walks. At most `max_results` rows (truncation: `NOTICE`,
/// `last_stats()`); a walk that reports a row per walk defaults to 100,000
/// and may not ask for more than `warren.max_walk_paths`. The walk runs
/// before the first row is returned; each row's external ids are made as
/// it is returned.
#[allow(clippy::too_many_arguments)] // the SQL signature of DESIGN.md §9.2
#[allow(clippy::type_complexity)] // pgrx reads the column names from name!() in place
#[pg_extern(
    schema = "warren",
    requires = ["walk_step"],
    sql = r#"
CREATE FUNCTION warren.walk(graph text, start anyarray, steps warren.step[],
                            mode text DEFAULT 'ends', distinct_ends boolean DEFAULT true,
                            max_results bigint DEFAULT NULL, reverse boolean DEFAULT false,
                            version bigint DEFAULT NULL)
RETURNS TABLE (end_id anyelement, path anyarray, rels text[], count bigint)
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'walk_wrapper';
"#
)]
fn walk<'a>(
    graph: Option<&'a str>,
    start: Option<AnyArray>,
    steps: Option<Array<'a, pgrx::composite_type!('a, "warren.step")>>,
    mode: Option<&'a str>,
    distinct_ends: Option<bool>,
    max_results: Option<i64>,
    reverse: Option<bool>,
    version: Option<i64>,
) -> TableIterator<
    'static,
    (
        name!(end_id, Option<AnyElement>),
        name!(path, Option<AnyArray>),
        name!(rels, Option<Vec<String>>),
        name!(count, Option<i64>),
    ),
> {
    entry(|| {
        let graph = graph.ok_or(TraversalSqlError::NullArgument("graph"))?;
        let steps = steps.ok_or(TraversalSqlError::NullArgument("steps"))?;
        let mode = walk_mode(mode)?;
        let distinct_ends =
            distinct_ends.ok_or(TraversalSqlError::NullArgument("distinct_ends"))?;
        let per_walk = mode == WalkMode::Paths || (mode == WalkMode::Ends && !distinct_ends);
        let options = WalkOptions {
            distinct_ends,
            max_results: row_limit(per_walk, max_results, max_walk_paths())?,
            reverse: reverse.ok_or(TraversalSqlError::NullArgument("reverse"))?,
        };
        let resolved = resolve(graph, version, "read")?;
        let mut recorder = Recorder::start("walk", graph, resolved.version);
        let rows = with_id_type!(resolved.graph.id_type, E => {
            let loaded = cached::<E>(&resolved)?;
            let starts = match start {
                Some(start) => dense_ids(&loaded.id_map, &elements::<E>(graph, "start", start)?),
                None => Vec::new(),
            };
            let steps = read_steps::<E>(&loaded, &steps)?;
            let walked =
                run_walk(&loaded.graph, &starts, &steps, mode, options, &mut PostgresInterrupts)?;
            recorder.record(walked.stats);
            walk_rows(loaded, walked.result, options.reverse)?
        });
        Ok(TableIterator::new(rows))
    })
}

/// `mode`: `ends`, `count` or `paths` (case-insensitive).
fn walk_mode(text: Option<&str>) -> Result<WalkMode, TraversalSqlError> {
    let text = text.ok_or(TraversalSqlError::NullArgument("mode"))?;
    match text.to_ascii_lowercase().as_str() {
        "ends" => Ok(WalkMode::Ends),
        "count" => Ok(WalkMode::Count),
        "paths" => Ok(WalkMode::Paths),
        _ => Err(TraversalSqlError::Mode(text.to_owned())),
    }
}

/// The row limit a walk runs with. A walk that reports a row per walk
/// (`per_walk`) is always bounded: by `max_results`, which may not exceed
/// `ceiling`, or else by [`DEFAULT_WALK_ROWS`] (or `ceiling`, if lower).
fn row_limit(
    per_walk: bool,
    max_results: Option<i64>,
    ceiling: Bound,
) -> Result<Option<u64>, TraversalSqlError> {
    let requested = bound::<u64>("max_results", max_results)?;
    if !per_walk {
        return Ok(requested);
    }
    let Bound { value, setting } = ceiling;
    let ceiling = i64::try_from(value).unwrap_or(i64::MAX);
    match max_results {
        Some(requested) if requested > ceiling => {
            Err(TraversalSqlError::WalkRowCeiling { requested, ceiling, setting })
        }
        Some(_) => Ok(requested),
        None => bound("max_results", Some(DEFAULT_WALK_ROWS.min(ceiling))),
    }
}

/// The rows of a walk's `result`, held as dense ids in the extension's
/// buffer, each row's external ids made when it is returned (DESIGN.md
/// §12), in the context Postgres resets between rows. The rows hold the
/// loaded version, which keeps it mapped until the call ends, however it
/// ends.
fn walk_rows<E: CachedIds>(
    loaded: Arc<LoadedVersion<E>>,
    result: WalkResult,
    reverse: bool,
) -> Result<Box<dyn Iterator<Item = WalkRow>>, TraversalSqlError> {
    Ok(match result {
        WalkResult::Ends(ends) => Box::new(
            ends.into_iter().map(move |end| (external(&loaded.id_map, end), None, None, None)),
        ),
        WalkResult::Paths(paths) => {
            Box::new(paths.into_iter().map(move |path| path_row(&loaded, &path, reverse)))
        }
        WalkResult::Count(count) => {
            let count =
                i64::try_from(count).map_err(|_| TraversalSqlError::CountOutOfRange(count))?;
            Box::new(std::iter::once((None, None, None, Some(count))))
        }
    })
}

/// A path's row: its reported node (the end, or in `reverse` the would-be
/// start), its nodes and its hops' relation names.
fn path_row<E: CachedIds>(loaded: &LoadedVersion<E>, path: &WalkPath, reverse: bool) -> WalkRow {
    let id_map = &loaded.id_map;
    let reported = match reverse {
        true => path.nodes.first(),
        false => path.nodes.last(),
    };
    let relations = &loaded.relations;
    let rels = path
        .relations
        .iter()
        .map(|&relation| relations.name(relation).unwrap_or_default().to_owned())
        .collect();
    (
        reported.and_then(|&node| external(id_map, node)),
        external_array(id_map, &path.nodes),
        Some(rels),
        None,
    )
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod unit {
    use super::*;

    #[test]
    fn walk_modes_are_ends_count_or_paths() {
        assert_eq!(walk_mode(Some("ends")).ok(), Some(WalkMode::Ends));
        assert_eq!(walk_mode(Some("COUNT")).ok(), Some(WalkMode::Count));
        assert_eq!(walk_mode(Some("paths")).ok(), Some(WalkMode::Paths));
        assert!(matches!(walk_mode(Some("all")), Err(TraversalSqlError::Mode(m)) if m == "all"));
    }

    #[test]
    fn a_walk_reporting_a_row_per_walk_is_always_bounded() {
        let ceiling = |value| Bound { value, setting: "warren.max_walk_paths" };
        assert_eq!(row_limit(false, None, ceiling(10)).ok(), Some(None));
        assert_eq!(row_limit(false, Some(50), ceiling(10)).ok(), Some(Some(50)));
        assert_eq!(row_limit(true, None, ceiling(10_000_000)).ok(), Some(Some(100_000)));
        assert_eq!(row_limit(true, None, ceiling(10)).ok(), Some(Some(10)));
        assert_eq!(row_limit(true, Some(7), ceiling(10)).ok(), Some(Some(7)));
        assert!(matches!(
            row_limit(true, Some(11), ceiling(10)),
            Err(TraversalSqlError::WalkRowCeiling {
                requested: 11,
                ceiling: 10,
                setting: "warren.max_walk_paths"
            })
        ));
        assert!(matches!(
            row_limit(true, Some(-1), ceiling(10)),
            Err(TraversalSqlError::Negative(_))
        ));
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::ceiling::set_in_configuration;
    use crate::fixture::{failure, held_after_first_row, iri_star, rows, small};

    // small: 1 → 2 (a), 2 → 3 (a), 3 → 4 (b), 1 → 4 (b), 4 → 1 (a), 5 → 5 (b).

    fn ends(steps: &str, rest: &str) -> Result<Option<String>, Error> {
        rows(&format!(
            "SELECT end_id::text FROM warren.walk('t', ARRAY[1::bigint], {steps}{rest}) ORDER BY 1"
        ))
    }

    #[pg_test]
    fn a_walk_follows_each_step_in_turn() -> Result<(), Error> {
        small("t")?;
        let aa = "ARRAY[warren.s(ARRAY['a']), warren.s(ARRAY['a'])]";
        assert_eq!(ends(aa, "")?.as_deref(), Some("3"));
        assert_eq!(
            ends("ARRAY[warren.s(ARRAY['a'], 'out', NULL, 1, 3)]", "")?.as_deref(),
            Some("2 3")
        );
        assert_eq!(
            ends("ARRAY[warren.s(NULL, 'out', ARRAY[4]::bigint[])]", "")?.as_deref(),
            Some("4")
        );
        assert_eq!(ends("ARRAY[]::warren.step[]", "")?.as_deref(), Some("1"));
        let jsonb = "'[{\"rels\": [\"a\"]}, {\"rels\": [\"a\"]}]'::jsonb::warren.step[]";
        assert_eq!(ends(jsonb, "")?.as_deref(), Some("3"));
        Ok(())
    }

    #[pg_test]
    fn paths_carry_nodes_and_relations_and_reverse_reports_starts() -> Result<(), Error> {
        small("t")?;
        let found = rows(
            "SELECT end_id || ' ' || path::text || ' ' || rels::text
             FROM warren.walk('t', ARRAY[1::bigint],
                              ARRAY[warren.s(ARRAY['a']), warren.s(ARRAY['a'])], 'paths')",
        )?;
        assert_eq!(found.as_deref(), Some("3 {1,2,3} {a,a}"));
        let found = rows(
            "SELECT end_id || ' ' || path::text
             FROM warren.walk('t', ARRAY[3::bigint],
                              ARRAY[warren.s(ARRAY['a']), warren.s(ARRAY['a'])], 'paths',
                              reverse := true)",
        )?;
        assert_eq!(found.as_deref(), Some("1 {1,2,3}"));
        let count = Spi::get_one::<i64>(
            "SELECT count FROM warren.walk('t', ARRAY[1::bigint],
                                           ARRAY[warren.s(NULL, 'out', NULL, 1, 2)], 'count',
                                           distinct_ends := false)",
        )?;
        assert_eq!(count, Some(4), "1→2, 1→4, 1→2→3, 1→4→1");
        Ok(())
    }

    /// Each row's text ids are made when the row is returned, so a walk
    /// holds no more Postgres memory for 50,000 rows to come than for
    /// 1,000, in either mode that returns ids: the walk's result stays as
    /// dense ids in the extension's buffer.
    #[pg_test]
    fn a_walk_makes_each_rows_ids_as_it_is_returned() -> Result<(), Error> {
        iri_star("few", 1_000)?;
        iri_star("many", 50_000)?;
        for mode in ["ends", "paths"] {
            let call = |graph: &str| {
                format!(
                    "warren.walk('{graph}', ARRAY['https://example.org/resource/0'::text],
                                 ARRAY[warren.s(NULL)], '{mode}')"
                )
            };
            let few = held_after_first_row(&call("few"))?;
            let many = held_after_first_row(&call("many"))?;
            assert!(
                many < few + (256 << 10),
                "{mode}: {many} bytes held for 50,000 rows, {few} for 1,000"
            );
        }
        Ok(())
    }

    #[pg_test]
    fn max_results_truncates_a_walk() -> Result<(), Error> {
        small("t")?;
        let found = ends("ARRAY[warren.s(NULL, 'out', NULL, 1, 2)]", ", max_results := 2")?;
        assert_eq!(found.map(|f| f.split(' ').count()), Some(2));
        let truncated = Spi::get_one::<bool>("SELECT (warren.last_stats() ->> 'truncated')::bool")?;
        assert_eq!(truncated, Some(true));
        Ok(())
    }

    #[pg_test(
        error = "max_results 11 is above warren.max_walk_paths (10) for a walk that reports a row per walk"
    )]
    fn per_walk_rows_are_capped_by_a_guc() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "SET LOCAL warren.max_walk_paths = 10;
             SELECT * FROM warren.walk('t', ARRAY[1::bigint], ARRAY[warren.s(NULL)], 'paths',
                                       max_results := 11)",
        )
    }

    /// Where `warren.walk_paths_limit` is lower than the session's
    /// `warren.max_walk_paths`, it bounds the walk, and the refusal names it.
    #[pg_test]
    fn per_walk_rows_are_capped_by_the_administrators_limit() -> Result<(), Error> {
        small("t")?;
        set_in_configuration(c"warren.walk_paths_limit", c"10");
        let refused = failure(
            "warren.walk('t', ARRAY[1::bigint], ARRAY[warren.s(NULL)], 'paths', max_results := 11)",
        );
        set_in_configuration(c"warren.walk_paths_limit", c"-1");
        assert_eq!(
            refused?.as_deref(),
            Some(
                "54000: max_results 11 is above warren.walk_paths_limit (10) for a walk that \
                 reports a row per walk"
            )
        );
        Ok(())
    }

    #[pg_test(error = "mode must be 'ends', 'count' or 'paths', not 'all'")]
    fn an_unknown_mode_is_an_error() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT * FROM warren.walk('t', ARRAY[1::bigint], ARRAY[warren.s(NULL)], 'all')")
    }

    #[pg_test(error = "hop range 3..2 is invalid: need min <= max <= 1024")]
    fn an_invalid_hop_range_is_an_error() -> Result<(), Error> {
        small("t")?;
        Spi::run("SELECT * FROM warren.walk('t', ARRAY[1::bigint], ARRAY[warren.s(NULL, 'out', NULL, 3, 2)])")
    }

    #[pg_test(error = "invalid input syntax for type bigint: \"x\"")]
    fn filter_ids_must_parse_as_the_graphs_id_type() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "SELECT * FROM warren.walk('t', ARRAY[1::bigint],
                                       ARRAY[warren.s(NULL, 'out', ARRAY['x'])])",
        )
    }

    /// Two nodes, each with an edge to itself and to the other: 2^k walks
    /// of k hops from either.
    fn doubling() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE dbl_nodes AS SELECT generate_series(1, 2)::bigint AS id;
             CREATE TABLE dbl_edges AS SELECT a.id AS src, b.id AS dst FROM dbl_nodes a, dbl_nodes b;
             SELECT warren.create_graph('dbl', 'SELECT id FROM dbl_nodes',
                                        'SELECT src, dst FROM dbl_edges');
             SELECT warren.refresh('dbl')",
        )
    }

    fn walk_count(hops: u32) -> String {
        format!(
            "(SELECT count FROM warren.walk('dbl', ARRAY[1::bigint],
                 ARRAY[warren.s(NULL, 'out', NULL, {hops}, {hops})], 'count',
                 distinct_ends := false))"
        )
    }

    #[pg_test]
    fn a_walk_count_beyond_bigint_is_an_error_not_a_clamp() -> Result<(), Error> {
        doubling()?;
        let count = Spi::get_one::<i64>(&format!("SELECT {}", walk_count(62)))?;
        assert_eq!(count, Some(1 << 62));
        assert_eq!(
            failure(&walk_count(63))?.as_deref(),
            Some("22003: walk count 9223372036854775808 exceeds the range of bigint")
        );
        Ok(())
    }

    #[pg_test]
    fn the_jsonb_cast_refuses_malformed_steps() -> Result<(), Error> {
        let cases = [
            ("'[{\"rels\": \"b\"}]'", "22023: step 1: rels must be an array or null, not string"),
            ("'[{\"filter\": 5}]'", "22023: step 1: filter must be an array or null, not number"),
            ("'[{\"relations\": [\"b\"]}]'", "22023: step 1 has an unknown key \"relations\""),
            ("'[{}, 5]'", "22023: step 2 must be an object, not number"),
            ("'{}'", "22023: steps must be a JSON array of step objects, not object"),
        ];
        for (json, expected) in cases {
            let found = failure(&format!("{json}::jsonb::warren.step[]"))?;
            assert_eq!(found.as_deref(), Some(expected), "{json}");
        }
        let fine = Spi::get_one::<String>(
            "SELECT ('[{\"rels\": null, \"filter\": null, \"direction\": \"in\"}]'::jsonb
                     ::warren.step[])::text",
        )?;
        assert_eq!(fine.as_deref(), Some("{\"(,in,,1,)\"}"));
        Ok(())
    }

    #[pg_test]
    fn a_step_filter_must_be_an_array() -> Result<(), Error> {
        for (filter, found) in [("5::bigint", "bigint"), ("'{1,2}'", "unknown")] {
            assert_eq!(
                failure(&format!("warren.s(NULL, 'out', {filter})"))?,
                Some(format!(
                    "42804: filter must be an array (e.g. ARRAY[...]::bigint[]), not {found}"
                ))
            );
        }
        Ok(())
    }
}
