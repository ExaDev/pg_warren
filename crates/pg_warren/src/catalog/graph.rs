//! Graph definitions: the rows of `warren.graphs` (DESIGN.md §5.1).

use pgrx::prelude::*;

use super::CatalogError;
use crate::access::as_extension_owner;

/// A graph's definition, as `warren.create_graph` recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphDefinition {
    /// `graphs.graph_id`.
    pub(crate) graph_id: i32,
    /// `graphs.name`.
    pub(crate) name: String,
    /// The owner role: source queries run as this role (DESIGN.md §7).
    pub(crate) owner: pg_sys::Oid,
    /// Roles whose members may query the graph (DESIGN.md §7).
    pub(crate) readers: Vec<pg_sys::Oid>,
    /// `SELECT <id> ...`.
    pub(crate) node_query: String,
    /// `SELECT <src>, <dst> [, <rel>] [, <weight>] ...`.
    pub(crate) edge_query: String,
    /// Whether edges are directed; undirected edges are stored both ways.
    pub(crate) directed: bool,
    /// Whether the edge query has a weight column.
    pub(crate) weighted: bool,
    /// Whether the edge query has a relation column.
    pub(crate) has_relations: bool,
    /// Whether to build the reverse CSRs.
    pub(crate) build_reverse: bool,
    /// Promoted versions kept, the current one included.
    pub(crate) retain_versions: i32,
    /// The promoted version builds start from, if any.
    pub(crate) current_version: Option<i64>,
}

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

impl GraphDefinition {
    /// The definition of graph `name`.
    pub(crate) fn find(name: &str) -> Result<Self, CatalogError> {
        as_extension_owner(|| {
            Spi::connect(|client| {
                let row = client
                    .select(
                        "SELECT graph_id, owner::oid, readers::oid[], node_query, edge_query,
                                directed, weighted, has_relations, build_reverse,
                                retain_versions, current_version
                         FROM warren.graphs WHERE name = $1",
                        Some(1),
                        &[name.into()],
                    )?
                    .first();
                if row.is_empty() {
                    return Err(CatalogError::GraphNotFound(name.to_owned()));
                }
                Ok(Self {
                    graph_id: row.get(1)?.unwrap_or_default(),
                    name: name.to_owned(),
                    owner: row.get(2)?.unwrap_or(pg_sys::Oid::INVALID),
                    readers: row.get(3)?.unwrap_or_default(),
                    node_query: row.get(4)?.unwrap_or_default(),
                    edge_query: row.get(5)?.unwrap_or_default(),
                    directed: row.get(6)?.unwrap_or_default(),
                    weighted: row.get(7)?.unwrap_or_default(),
                    has_relations: row.get(8)?.unwrap_or_default(),
                    build_reverse: row.get(9)?.unwrap_or_default(),
                    retain_versions: row.get(10)?.unwrap_or(1),
                    current_version: row.get(11)?,
                })
            })
        })
    }

    /// Locks the graph's row until the end of the transaction, so that
    /// refreshes of one graph run one at a time, and re-reads its current
    /// version, which a refresh that held the lock may have changed.
    pub(crate) fn lock(&mut self) -> Result<(), CatalogError> {
        self.current_version = as_extension_owner(|| {
            Ok::<_, CatalogError>(Spi::get_one_with_args::<i64>(
                "SELECT current_version FROM warren.graphs WHERE graph_id = $1 FOR UPDATE",
                &[self.graph_id.into()],
            )?)
        })?;
        Ok(())
    }

    /// Deletes the graph, and with it every version, segment and vector.
    pub(crate) fn drop_graph(&self) -> Result<(), CatalogError> {
        as_extension_owner(|| {
            Spi::connect_mut(|client| {
                client.update(
                    "DELETE FROM warren.graphs WHERE graph_id = $1",
                    None,
                    &[self.graph_id.into()],
                )?;
                Ok(())
            })
        })
    }

    /// Adds `role` to the readers (if `add`) or removes it, pruning reader
    /// roles that no longer exist.
    pub(crate) fn set_reader(&self, role: pg_sys::Oid, add: bool) -> Result<(), CatalogError> {
        as_extension_owner(|| {
            Spi::connect_mut(|client| {
                client.update(
                    "UPDATE warren.graphs SET readers = ARRAY(
                         SELECT DISTINCT r.oid FROM pg_roles r
                         WHERE r.oid = ANY (readers::oid[] || CASE WHEN $3 THEN $2 END)
                           AND r.oid <> CASE WHEN $3 THEN 0::oid ELSE $2 END
                         ORDER BY r.oid)::regrole[]
                     WHERE graph_id = $1",
                    None,
                    &[self.graph_id.into(), role.into(), add.into()],
                )?;
                Ok(())
            })
        })
    }
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
