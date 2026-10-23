//! `warren.info`: a graph's definition, its versions and their statistics,
//! and for a contraction what each version was contracted from, as `jsonb`
//! (DESIGN.md §9.3, §6.6). Owners and readers may call it.

use pgrx::prelude::*;
use pgrx::JsonB;

use super::{CatalogError, GraphDefinition};
use crate::access::{as_catalog_owner, entry, is_reader, require_reader};
use crate::read_only::read_one;

/// `warren.info`: graph `name`'s definition (with its default quota on
/// readers' vectors, `reader_vector_quota`), current version, staleness
/// (`now() - built_at` of the current version), total bytes, and every
/// version with its statistics, oldest first.
#[pg_extern(schema = "warren", stable, parallel_safe)]
fn info(name: &str) -> JsonB {
    entry(|_| describe(name))
}

fn describe(name: &str) -> Result<JsonB, CatalogError> {
    let graph = GraphDefinition::find(name)?;
    require_reader(graph.owner, &graph.readers, &graph.name, "read")?;
    let info = as_catalog_owner(|| {
        Ok::<_, CatalogError>(read_one::<JsonB>(
            "SELECT jsonb_build_object(
                 'name', g.name, 'source', g.source, 'owner', g.owner::text, 'readers', to_jsonb(g.readers::text[]),
                 'id_type', g.id_type::text, 'search_path', g.search_path, 'directed', g.directed, 'weighted', g.weighted,
                 'has_relations', g.has_relations, 'build_reverse', g.build_reverse,
                 'store_merged', g.store_merged, 'node_order', g.node_order,
                 'retain_versions', g.retain_versions, 'current_version', g.current_version,
                 'reader_vector_quota', g.reader_vector_quota,
                 'staleness', (SELECT (now() - c.built_at)::text FROM warren.versions c
                               WHERE c.version_id = g.current_version
                                 AND c.graph_id = g.graph_id),
                 'bytes', (SELECT coalesce(sum(v.bytes), 0) FROM warren.versions v
                           WHERE v.graph_id = g.graph_id),
                 'versions', coalesce((
                     SELECT jsonb_agg(jsonb_build_object(
                         'version', v.version_id, 'seq', v.seq, 'staged', v.staged,
                         'current', v.version_id IS NOT DISTINCT FROM g.current_version,
                         'label', v.label, 'built_at', v.built_at,
                         'built_lsn', v.built_lsn::text, 'staleness', (now() - v.built_at)::text,
                         'node_count', v.node_count, 'id_space', v.id_space,
                         'edge_count', v.edge_count, 'dropped_edges', v.dropped_edges,
                         'deduplicated_edges', v.deduplicated_edges,
                         'target_width', v.target_width, 'bytes', v.bytes,
                         'compacted_from', v.compacted_from,
                         'parent_version', v.parent_version,
                         'parent_graph', (SELECT p.name FROM warren.versions pv
                                          JOIN warren.graphs p ON p.graph_id = pv.graph_id
                                          WHERE pv.version_id = v.parent_version),
                         'label_vector', v.label_vector,
                         'vectors', (SELECT count(*) FROM warren.vectors x
                                     WHERE x.version_id = v.version_id))
                         ORDER BY v.seq)
                     FROM warren.versions v WHERE v.graph_id = g.graph_id), '[]'))
             FROM warren.graphs g WHERE g.graph_id = $1",
            &[graph.graph_id.into()],
        )?)
    })?;
    let mut info = info.ok_or_else(|| CatalogError::GraphNotFound(name.to_owned()))?;
    hide_unreadable_parents(&mut info)?;
    Ok(info)
}

/// Clears what a version of a contraction says of its parent (the parent
/// graph, its version and its label vector) where the caller may not read
/// the parent graph: a quotient's reader learns nothing of a graph it
/// cannot read (DESIGN.md §6.6, §7).
fn hide_unreadable_parents(info: &mut JsonB) -> Result<(), CatalogError> {
    let Some(versions) = info.0.get_mut("versions").and_then(|v| v.as_array_mut()) else {
        return Ok(());
    };
    for version in versions {
        let Some(parent) = version.get("parent_graph").and_then(|p| p.as_str()).map(str::to_owned)
        else {
            continue;
        };
        let readable = match GraphDefinition::find(&parent) {
            Ok(graph) => is_reader(graph.owner, &graph.readers),
            Err(CatalogError::GraphNotFound(_)) => false,
            Err(error) => return Err(error),
        };
        if !readable {
            for key in ["parent_graph", "parent_version", "label_vector"] {
                version[key] = serde_json::Value::Null;
            }
        }
    }
    Ok(())
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{refresh, ring};

    /// A restored row can name another graph's version as current
    /// (DESIGN.md §5.1): info reports the value but no staleness for it.
    #[pg_test]
    fn info_reports_a_stray_current_version_without_its_staleness() -> Result<(), Error> {
        use crate::fixture::{set_current_unchecked, two_graphs};
        let (_, other) = two_graphs("istray")?;
        set_current_unchecked("istray_a", other)?;
        let reported = Spi::get_one::<String>(
            "SELECT format('%s|%s', i ->> 'current_version', i ->> 'staleness')
             FROM warren.info('istray_a') i",
        )?;
        assert_eq!(reported, Some(format!("{other}|")));
        Ok(())
    }

    fn info(path: &str) -> Result<Option<String>, Error> {
        Spi::get_one::<String>(&format!("SELECT (warren.info('i') #> '{path}')::text"))
    }

    #[pg_test]
    fn info_reports_the_definition_and_every_version() -> Result<(), Error> {
        ring("i", 4, 2)?;
        let promoted = refresh("i", true)?;
        let staged = refresh("i", false)?;
        assert_eq!(info("{name}")?.as_deref(), Some("\"i\""));
        assert_eq!(info("{source}")?.as_deref(), Some("\"query\""));
        assert_eq!(info("{versions,0,parent_version}")?.as_deref(), Some("null"));
        assert_eq!(info("{versions,0,parent_graph}")?.as_deref(), Some("null"));
        assert_eq!(info("{versions,0,label_vector}")?.as_deref(), Some("null"));
        assert_eq!(info("{owner}")?.as_deref(), Some("\"i_owner\""));
        assert_eq!(info("{id_type}")?.as_deref(), Some("\"bigint\""));
        assert_eq!(info("{readers}")?.as_deref(), Some("[]"));
        assert_eq!(info("{retain_versions}")?.as_deref(), Some("2"));
        assert_eq!(info("{store_merged}")?.as_deref(), Some("true"));
        assert_eq!(info("{node_order}")?.as_deref(), Some("\"id\""));
        assert_eq!(info("{reader_vector_quota}")?.as_deref(), Some("null"));
        Spi::run("SELECT warren.set_vector_quota('i', 4096)")?;
        assert_eq!(info("{reader_vector_quota}")?.as_deref(), Some("4096"));
        assert_eq!(info("{current_version}")?, Some(promoted.to_string()));
        assert_eq!(info("{versions,1,version}")?, Some(staged.to_string()));
        assert_eq!(info("{versions,0,current}")?.as_deref(), Some("true"));
        assert_eq!(info("{versions,1,staged}")?.as_deref(), Some("true"));
        assert_eq!(info("{versions,0,node_count}")?.as_deref(), Some("4"));
        assert_eq!(info("{versions,0,edge_count}")?.as_deref(), Some("4"));
        let bytes = Spi::get_one::<bool>(
            "SELECT (warren.info('i') ->> 'bytes')::bigint = (SELECT sum(bytes) FROM warren.versions)
                AND (warren.info('i') ->> 'staleness')::interval >= '0'
                AND (warren.info('i') #>> '{versions,0,built_lsn}')::pg_lsn IS NOT NULL",
        )?;
        assert_eq!(bytes, Some(true));
        Ok(())
    }

    #[pg_test]
    fn a_graph_never_built_has_no_versions() -> Result<(), Error> {
        ring("i", 4, 2)?;
        assert_eq!(info("{versions}")?.as_deref(), Some("[]"));
        assert_eq!(info("{current_version}")?.as_deref(), Some("null"));
        assert_eq!(info("{staleness}")?.as_deref(), Some("null"));
        Ok(())
    }

    #[pg_test]
    fn a_reader_may_call_info() -> Result<(), Error> {
        ring("i", 4, 2)?;
        Spi::run("CREATE ROLE info_reader; SELECT warren.grant('i', 'info_reader')")?;
        Spi::run("SET ROLE info_reader")?;
        let name = info("{name}");
        Spi::run("RESET ROLE")?;
        assert_eq!(name?.as_deref(), Some("\"i\""));
        Ok(())
    }

    #[pg_test(
        error = "must be a member of role \"i_owner\" or of a reader role to read graph \"i\""
    )]
    fn a_role_neither_owner_nor_reader_may_not_call_info() -> Result<(), Error> {
        ring("i", 4, 2)?;
        Spi::run("CREATE ROLE info_stranger; SET ROLE info_stranger; SELECT warren.info('i')")
    }
}
