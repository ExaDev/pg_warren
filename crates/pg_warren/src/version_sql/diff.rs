//! `warren.diff`: the structural delta between two versions of a graph in
//! the same id space, as `jsonb` (DESIGN.md §7, §8).

use pgrx::prelude::*;
use pgrx::JsonB;
use serde_json::{json, Value};
use warren_core::version::{diff as delta, DegreeShift, EdgeDelta, VersionDiff};

use super::error::VersionOperationError;
use crate::access::require_reader;
use crate::cache::load;
use crate::catalog::{compaction_between, require_version, GraphDefinition, VersionId};
use crate::extension::or_raise;
use crate::segment::SpiSegmentStore;

/// `warren.diff`: the structural delta from version `v1` of graph `name` to
/// version `v2` — nodes added and removed, edges added and removed per
/// relation and over all relations, and the shift of the out- and in-degree
/// distributions. Refused across a compaction boundary. Owners and readers
/// may call it.
#[pg_extern(schema = "warren", stable)]
fn diff(name: &str, v1: i64, v2: i64) -> JsonB {
    or_raise(diff_versions(name, v1, v2))
}

fn diff_versions(name: &str, v1: VersionId, v2: VersionId) -> Result<JsonB, VersionOperationError> {
    let graph = GraphDefinition::find(name)?;
    require_reader(graph.owner, &graph.readers, &graph.name, "read")?;
    require_version(&graph, v1)?;
    require_version(&graph, v2)?;
    if let Some((compacted, source_version)) = compaction_between(&graph, v1, v2)? {
        return Err(VersionOperationError::CompactionBoundary {
            graph: graph.name,
            from: v1,
            to: v2,
            compacted,
            source_version,
        });
    }
    let before = load::<i64>(&mut SpiSegmentStore, v1)?;
    let after = load::<i64>(&mut SpiSegmentStore, v2)?;
    let delta = delta(&before.version()?, &after.version()?)?;
    Ok(JsonB(diff_json(v1, v2, &delta)))
}

/// A diff as JSON: `from`, `to`, `nodes`, `relations` (one entry per
/// relation, named), `all_relations`, `out_degree` and `in_degree`.
fn diff_json(from: VersionId, to: VersionId, diff: &VersionDiff) -> Value {
    let nodes = &diff.nodes;
    json!({
        "from": from,
        "to": to,
        "nodes": {
            "before": nodes.before, "after": nodes.after,
            "added": nodes.added, "removed": nodes.removed,
        },
        "relations": diff.relations.iter().map(|relation| {
            let mut entry = edges_json(&relation.edges);
            entry["relation"] = json!(relation.name);
            entry
        }).collect::<Vec<_>>(),
        "all_relations": edges_json(&diff.all_relations),
        "out_degree": degree_json(&diff.out_degree),
        "in_degree": degree_json(&diff.in_degree),
    })
}

fn edges_json(edges: &EdgeDelta) -> Value {
    json!({
        "before": edges.before, "after": edges.after,
        "added": edges.added, "removed": edges.removed,
    })
}

fn degree_json(shift: &DegreeShift) -> Value {
    json!({
        "mean_before": shift.mean_before,
        "mean_after": shift.mean_after,
        "ks_statistic": shift.ks_statistic,
        "histogram": shift.histogram.iter().map(|count| json!({
            "degree": count.degree, "before": count.before, "after": count.after,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod unit {
    use super::*;
    use warren_core::graph::{EdgeIntake, GraphCsr, IdMap, RelationDictionary};
    use warren_core::version::{diff, GraphVersion};

    struct Built {
        id_map: IdMap<i64>,
        relations: RelationDictionary,
        graph: GraphCsr,
    }

    fn built(previous: Option<&Built>, nodes: &[i64], edges: &[(&str, i64, i64)]) -> Built {
        let base = previous.map(|b| b.id_map.clone()).unwrap_or_default();
        let id_map = base.rebuild(nodes.to_vec()).unwrap();
        let mut relations = previous.map(|b| b.relations.clone()).unwrap_or_default();
        let mut intake = EdgeIntake::directed(&id_map, false);
        for (name, source, target) in edges {
            let relation = relations.intern(name).unwrap();
            intake.push(relation, Some(source), Some(target), 1.0).unwrap();
        }
        let graph =
            GraphCsr::build(intake.finish().0, relations.len(), id_map.id_space(), false).unwrap();
        Built { id_map, relations, graph }
    }

    fn view(b: &Built) -> GraphVersion<'_, i64> {
        GraphVersion::new(&b.id_map, &b.relations, &b.graph).unwrap()
    }

    #[test]
    fn a_diff_becomes_jsonb_with_every_delta() {
        let before = built(None, &[1, 2, 3], &[("a", 1, 2), ("b", 2, 3)]);
        let after = built(Some(&before), &[1, 2, 4], &[("a", 1, 2), ("a", 4, 1)]);
        let delta = diff(&view(&before), &view(&after)).unwrap();
        let json = diff_json(7, 8, &delta);
        assert_eq!(json["from"], 7);
        assert_eq!(json["to"], 8);
        assert_eq!(
            json["nodes"],
            serde_json::json!({"before": 3, "after": 3, "added": 1, "removed": 1})
        );
        assert_eq!(
            json["relations"],
            serde_json::json!([
                {"relation": "a", "before": 1, "after": 2, "added": 1, "removed": 0},
                {"relation": "b", "before": 1, "after": 0, "added": 0, "removed": 1},
            ])
        );
        assert_eq!(json["all_relations"]["added"], 1);
        assert_eq!(
            json["out_degree"]["histogram"][0],
            serde_json::json!({"degree": 0, "before": 1, "after": 1})
        );
        assert!(json["in_degree"]["ks_statistic"].is_number());
        assert!(json["out_degree"]["mean_before"].is_number());
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{refresh, ring};

    #[pg_test]
    fn diff_reports_the_delta_between_a_staged_and_the_current_version() -> Result<(), Error> {
        ring("df", 5, 2)?;
        let current = refresh("df", true)?;
        Spi::run(
            "INSERT INTO df_nodes VALUES (6);
             INSERT INTO df_edges VALUES (5, 6);
             DELETE FROM df_edges WHERE src = 1",
        )?;
        let staged = refresh("df", false)?;
        let delta = Spi::get_one::<String>(&format!(
            "SELECT format('%s %s %s %s', d -> 'nodes' ->> 'added', d -> 'all_relations' ->> 'added',
                           d -> 'all_relations' ->> 'removed', d ->> 'to')
             FROM warren.diff('df', {current}, {staged}) d"
        ))?;
        assert_eq!(delta, Some(format!("1 1 1 {staged}")));
        Ok(())
    }

    #[pg_test]
    fn diff_refuses_versions_separated_by_a_compaction() -> Result<(), Error> {
        ring("df", 5, 3)?;
        let first = refresh("df", true)?;
        Spi::run("DELETE FROM df_nodes WHERE id = 3")?;
        let second = refresh("df", true)?;
        let compacted = Spi::get_one::<i64>("SELECT warren.compact('df')")?.unwrap_or_default();
        let refused = Spi::get_one::<bool>(&format!(
            "DO $$ BEGIN PERFORM warren.diff('df', {first}, {compacted}); RAISE 'diffed';
             EXCEPTION WHEN invalid_parameter_value THEN NULL; END $$; SELECT true"
        ));
        assert_eq!(refused, Ok(Some(true)));
        let diffed = Spi::get_one::<i64>(&format!(
            "SELECT (warren.diff('df', {first}, {second}) -> 'nodes' ->> 'removed')::bigint"
        ))?;
        assert_eq!(diffed, Some(1));
        Ok(())
    }

    #[pg_test(
        error = "versions 1 and 3 of graph \"df\" are in different id spaces: version 3 was compacted from version 2"
    )]
    fn the_compaction_boundary_is_named() -> Result<(), Error> {
        ring("df", 5, 3)?;
        Spi::run("SELECT setval('warren.versions_version_id_seq', 1, false)")?;
        refresh("df", true)?;
        Spi::run("DELETE FROM df_nodes WHERE id = 3")?;
        refresh("df", true)?;
        Spi::run("SELECT warren.compact('df'); SELECT warren.diff('df', 1, 3)")
    }

    #[pg_test(error = "graph \"df\" has no version 999")]
    fn diffing_an_unknown_version_fails() -> Result<(), Error> {
        ring("df", 5, 2)?;
        let current = refresh("df", true)?;
        Spi::run(&format!("SELECT warren.diff('df', {current}, 999)"))
    }

    #[pg_test]
    fn a_reader_may_diff() -> Result<(), Error> {
        ring("df", 5, 2)?;
        let v = refresh("df", true)?;
        Spi::run("CREATE ROLE diff_reader; SELECT warren.grant('df', 'diff_reader'); SET ROLE diff_reader")?;
        let delta = Spi::get_one::<i64>(&format!(
            "SELECT (warren.diff('df', {v}, {v}) -> 'nodes' ->> 'added')::bigint"
        ));
        Spi::run("RESET ROLE")?;
        assert_eq!(delta?, Some(0));
        Ok(())
    }
}
