//! `warren.info`: a graph's definition, its versions and their statistics,
//! as `jsonb` (DESIGN.md §9.3). Owners and readers may call it.

use pgrx::prelude::*;
use pgrx::JsonB;

use super::{CatalogError, GraphDefinition};
use crate::access::{as_extension_owner, entry, require_reader};
use crate::read_only::read_one;

/// `warren.info`: graph `name`'s definition, current version, staleness
/// (`now() - built_at` of the current version), total bytes, and every
/// version with its statistics, oldest first.
#[pg_extern(schema = "warren", stable, parallel_restricted)]
fn info(name: &str) -> JsonB {
    entry(|_| describe(name))
}

fn describe(name: &str) -> Result<JsonB, CatalogError> {
    let graph = GraphDefinition::find(name)?;
    require_reader(graph.owner, &graph.readers, &graph.name, "read")?;
    let info = as_extension_owner(|| {
        Ok::<_, CatalogError>(read_one::<JsonB>(
            "SELECT jsonb_build_object(
                 'name', g.name, 'owner', g.owner::text, 'readers', to_jsonb(g.readers::text[]),
                 'id_type', g.id_type::text, 'search_path', g.search_path, 'directed', g.directed, 'weighted', g.weighted,
                 'has_relations', g.has_relations, 'build_reverse', g.build_reverse,
                 'retain_versions', g.retain_versions, 'current_version', g.current_version,
                 'staleness', (SELECT (now() - c.built_at)::text FROM warren.versions c
                               WHERE c.version_id = g.current_version),
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
                         'vectors', (SELECT count(*) FROM warren.vectors x
                                     WHERE x.version_id = v.version_id))
                         ORDER BY v.seq)
                     FROM warren.versions v WHERE v.graph_id = g.graph_id), '[]'))
             FROM warren.graphs g WHERE g.graph_id = $1",
            &[graph.graph_id.into()],
        )?)
    })?;
    info.ok_or_else(|| CatalogError::GraphNotFound(name.to_owned()))
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{refresh, ring};

    fn info(path: &str) -> Result<Option<String>, Error> {
        Spi::get_one::<String>(&format!("SELECT (warren.info('i') #> '{path}')::text"))
    }

    #[pg_test]
    fn info_reports_the_definition_and_every_version() -> Result<(), Error> {
        ring("i", 4, 2)?;
        let promoted = refresh("i", true)?;
        let staged = refresh("i", false)?;
        assert_eq!(info("{name}")?.as_deref(), Some("\"i\""));
        assert_eq!(info("{owner}")?.as_deref(), Some("\"i_owner\""));
        assert_eq!(info("{id_type}")?.as_deref(), Some("\"bigint\""));
        assert_eq!(info("{readers}")?.as_deref(), Some("[]"));
        assert_eq!(info("{retain_versions}")?.as_deref(), Some("2"));
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
