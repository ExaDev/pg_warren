//! Graph definitions: the rows of `warren.graphs` (DESIGN.md §5.1).

use pgrx::prelude::*;

use super::{as_extension_owner, CatalogError};

/// A graph definition to record: the arguments of `warren.create_graph`,
/// plus the owner and the id type it determined.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NewGraph<'a> {
    pub(crate) name: &'a str,
    pub(crate) owner: pg_sys::Oid,
    pub(crate) node_query: &'a str,
    pub(crate) edge_query: &'a str,
    pub(crate) id_type: pg_sys::Oid,
    pub(crate) directed: bool,
    pub(crate) weighted: bool,
    pub(crate) has_relations: bool,
    pub(crate) build_reverse: bool,
    pub(crate) retain_versions: i32,
}

impl NewGraph<'_> {
    /// Records a new graph definition.
    pub(crate) fn create(&self) -> Result<(), CatalogError> {
        let graph = self;
        as_extension_owner(|| {
            Spi::connect_mut(|client| {
                let exists = client
                    .select(
                        "SELECT 1 FROM warren.graphs WHERE name = $1",
                        Some(1),
                        &[graph.name.into()],
                    )?
                    .len();
                if exists > 0 {
                    return Err(CatalogError::GraphExists(graph.name.to_owned()));
                }
                client.update(
                    "INSERT INTO warren.graphs (name, owner, node_query, edge_query, id_type,
                         directed, weighted, has_relations, build_reverse, retain_versions)
                     VALUES ($1, $2::oid::regrole, $3, $4, $5::oid::regtype, $6, $7, $8, $9, $10)",
                    None,
                    &[
                        graph.name.into(),
                        graph.owner.into(),
                        graph.node_query.into(),
                        graph.edge_query.into(),
                        graph.id_type.into(),
                        graph.directed.into(),
                        graph.weighted.into(),
                        graph.has_relations.into(),
                        graph.build_reverse.into(),
                        graph.retain_versions.into(),
                    ],
                )?;
                Ok(())
            })
        })
    }
}
